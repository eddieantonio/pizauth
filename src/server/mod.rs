mod http_server;
mod notifier;
mod refresher;
mod request_token;
mod state;

use std::{
    error::Error,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use log::warn;
use nix::sys::signal::{raise, Signal};
#[cfg(target_os = "openbsd")]
use pledge::pledge;
#[cfg(target_os = "openbsd")]
use unveil::unveil;

use crate::{config::Config, PIZAUTH_CACHE_SOCK_LEAF};
use notifier::Notifier;
use refresher::{RefreshKind, Refresher};
use request_token::request_token;
use state::{AuthenticatorState, CTGuard, CTGuardAccountId, TokenState};

/// Length of the PKCE code verifier in bytes.
const CODE_VERIFIER_LEN: usize = 64;
/// Length of the OAuth state in bytes.
const STATE_LEN: usize = 8;

pub fn sock_path(cache_path: &Path) -> PathBuf {
    let mut p = cache_path.to_owned();
    p.push(PIZAUTH_CACHE_SOCK_LEAF);
    p
}

fn request(pstate: Arc<AuthenticatorState>, mut stream: UnixStream) -> Result<(), Box<dyn Error>> {
    let mut cmd = String::new();
    stream.read_to_string(&mut cmd)?;

    match &cmd.split(' ').collect::<Vec<_>>()[..] {
        ["reload"] => {
            match Config::from_path(&pstate.conf_path) {
                Ok(new_conf) => {
                    pstate.update_conf(new_conf);
                    stream.write_all(b"ok:")?
                }
                Err(e) => stream.write_all(format!("error:{e:}").as_bytes())?,
            }
            Ok(())
        }
        ["refresh", with_url, act_name] => {
            let ct_lk = pstate.ct_lock();
            let act_id = match ct_lk.validate_act_name(act_name) {
                Some(x) => x,
                None => {
                    drop(ct_lk);
                    stream.write_all(format!("error:No account '{act_name:}'").as_bytes())?;
                    return Ok(());
                }
            };
            match ct_lk.tokenstate(&act_id) {
                TokenState::Empty | TokenState::Pending { .. } => {
                    let url = request_token(Arc::clone(&pstate), ct_lk, act_id)?;
                    if *with_url == "withurl" {
                        stream.write_all(format!("pending:{url:}").as_bytes())?;
                    } else {
                        stream.write_all(b"pending:")?;
                    }
                }
                TokenState::Active { .. } => {
                    match pstate.refresher.refresh(&pstate, ct_lk, act_id)? {
                        RefreshKind::AccountOrTokenStateChanged => stream.write_all(b"error:")?,
                        RefreshKind::PermanentError(msg) => {
                            stream.write_all(format!("error:{msg:}").as_bytes())?
                        }
                        RefreshKind::Refreshed => stream.write_all(b"ok:")?,
                        RefreshKind::TransitoryError(msg) => {
                            stream.write_all(format!("error:{msg:}").as_bytes())?
                        }
                    }
                }
            }
            Ok(())
        }
        ["showtoken", with_url, act_name] => {
            // If unwrap()ing the lock fails, we're in such deep trouble that trying to carry on is
            // pointless.
            let ct_lk = pstate.ct_lock();
            let act_id = match ct_lk.validate_act_name(act_name) {
                Some(x) => x,
                None => {
                    drop(ct_lk);
                    stream.write_all(format!("error:No account '{act_name:}'").as_bytes())?;
                    return Ok(());
                }
            };
            match ct_lk.tokenstate(&act_id) {
                TokenState::Empty => {
                    let url = request_token(Arc::clone(&pstate), ct_lk, act_id)?;
                    if *with_url == "withurl" {
                        stream.write_all(format!("pending:{url:}").as_bytes())?;
                    } else {
                        stream.write_all(b"pending:")?;
                    }
                }
                TokenState::Pending { ref url, .. } => {
                    let response = if *with_url == "withurl" {
                        format!("pending:{url:}")
                    } else {
                        "pending:".to_owned()
                    };
                    drop(ct_lk);
                    stream.write_all(response.as_bytes())?;
                }
                TokenState::Active {
                    access_token,
                    expiry,
                    refreshed_at: _,
                    last_refresh_attempt: _,
                    refresh_token: _,
                } => {
                    let response = if expiry > &Instant::now() {
                        format!("access_token:{access_token:}")
                    } else {
                        "error:Access token has expired and refreshing has not yet succeeded".into()
                    };
                    drop(ct_lk);
                    stream.write_all(response.as_bytes())?;
                }
            }
            Ok(())
        }
        ["shutdown"] => {
            raise(Signal::SIGTERM).ok();
            Ok(())
        }
        _ => Err(format!("Invalid cmd '{cmd:}'").into()),
    }
}

pub fn server(conf_path: PathBuf, conf: Config, cache_path: &Path) -> Result<(), Box<dyn Error>> {
    let sock_path = sock_path(cache_path);

    #[cfg(target_os = "openbsd")]
    unveil(
        conf_path
            .as_os_str()
            .to_str()
            .ok_or("Cannot use configuration path in unveil")?,
        "rx",
    )?;
    #[cfg(target_os = "openbsd")]
    unveil(
        sock_path
            .as_os_str()
            .to_str()
            .ok_or("Cannot use socket path in unveil")?,
        "rwxc",
    )?;
    #[cfg(target_os = "openbsd")]
    unveil(std::env::var("SHELL")?, "rx")?;
    #[cfg(target_os = "openbsd")]
    unveil("/dev/random", "rx")?;
    #[cfg(target_os = "openbsd")]
    unveil("", "")?;

    #[cfg(target_os = "openbsd")]
    pledge("stdio rpath wpath inet fattr unix dns proc exec", None).unwrap();

    let (http_port, http_state) = http_server::http_server_setup(&conf)?;
    let notifier = Arc::new(Notifier::new()?);
    let refresher = Refresher::new();

    let pstate = Arc::new(AuthenticatorState::new(
        conf_path,
        conf,
        http_port,
        Arc::clone(&notifier),
        Arc::clone(&refresher),
    ));

    http_server::http_server(Arc::clone(&pstate), http_state)?;
    refresher.refresher(Arc::clone(&pstate))?;
    notifier.notifier(Arc::clone(&pstate))?;

    let listener = UnixListener::bind(sock_path)?;
    for stream in listener.incoming().flatten() {
        let pstate = Arc::clone(&pstate);
        if let Err(e) = request(pstate, stream) {
            warn!("{e:}");
        }
    }

    Ok(())
}
