use std::{
    cmp,
    error::Error,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

#[cfg(debug_assertions)]
use log::debug;
use log::error;

use super::{AuthenticatorState, CTGuard, CTGuardAccountId, TokenState};

/// The outcome of an attempted refresh.
pub enum RefreshKind {
    /// Refreshing terminated because the config or tokenstate changed.
    AccountOrTokenStateChanged,
    /// Refreshing failed in a way that is likely to repeat if retried.
    PermanentError(String),
    /// The token was refreshed.
    Refreshed,
    /// Refreshing failed but in a way that is not likely to repeat if retried.
    TransitoryError(String),
}

pub struct Refresher {
    pred: Mutex<bool>,
    condvar: Condvar,
}

/// If there is an active token for `act_id`, refresh it, blocking until the token is refreshed or
/// an error occurred. If there is not an active token, returns `Ok` immediately.
pub fn refresh(
    pstate: Arc<AuthenticatorState>,
    ct_lk: CTGuard,
    act_id: CTGuardAccountId,
) -> Result<RefreshKind, Box<dyn Error>> {
    let refresh_token = match ct_lk.tokenstate(&act_id) {
        TokenState::Active {
            refresh_token: Some(refresh_token),
            ..
        } => refresh_token.to_owned(),
        _ => todo!(),
    };

    let act = ct_lk.account(&act_id);
    let token_uri = act.token_uri.clone();
    let client_id = act.client_id.clone();
    let client_secret = act.client_secret.clone();
    let pairs = [
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("refresh_token", refresh_token.as_str()),
        ("grant_type", "refresh_token"),
    ];

    drop(ct_lk);
    let body = match ureq::post(token_uri.as_str()).send_form(&pairs) {
        Ok(response) => match response.into_string() {
            Ok(s) => s,
            Err(e) => {
                return Ok(RefreshKind::TransitoryError(e.to_string()));
            }
        },
        Err(ureq::Error::Status(code, response)) => {
            let reason = match response.into_string() {
                Ok(r) => format!("{code:}: {r:}"),
                Err(_) => format!("{code:}"),
            };
            let mut ct_lk = pstate.ct_lock();
            match ct_lk.validate_act_id(act_id) {
                Some(act_id) => {
                    ct_lk.tokenstate_replace(act_id, TokenState::Empty);
                    return Ok(RefreshKind::PermanentError(reason));
                }
                None => return Ok(RefreshKind::AccountOrTokenStateChanged),
            }
        }
        Err(e) => return Ok(RefreshKind::TransitoryError(e.to_string())),
    };

    let parsed = json::parse(&body)?;
    if parsed["error"].as_str().is_some() {
        // Refreshing failed. Unfortunately there is no standard way of knowing why it failed, so
        // we take the most pessimistic assumption which is that the refresh token is no longer
        // valid at all.
        let mut ct_lk = pstate.ct_lock();
        match ct_lk.validate_act_id(act_id) {
            Some(act_id) => {
                let act_id = ct_lk.tokenstate_replace(act_id, TokenState::Empty);
                let msg = format!("Refreshing {} failed", ct_lk.account(&act_id).name);
                drop(ct_lk);
                return Ok(RefreshKind::PermanentError(msg));
            }
            None => return Ok(RefreshKind::AccountOrTokenStateChanged),
        }
    }

    match (
        parsed["access_token"].as_str(),
        parsed["expires_in"].as_u64(),
        parsed["token_type"].as_str(),
    ) {
        (Some(access_token), Some(expires_in), Some(token_type)) if token_type == "Bearer" => {
            let refreshed_at = Instant::now();
            let expiry = refreshed_at
                .checked_add(Duration::from_secs(expires_in))
                .ok_or("Can't represent expiry")?;
            let mut ct_lk = pstate.ct_lock();
            match ct_lk.validate_act_id(act_id) {
                Some(act_id) => {
                    ct_lk.tokenstate_replace(
                        act_id,
                        TokenState::Active {
                            access_token: access_token.to_owned(),
                            expiry,
                            refreshed_at,
                            last_refresh_attempt: Some(Instant::now()),
                            refresh_token: Some(refresh_token),
                        },
                    );
                    drop(ct_lk);
                    update_refresher(pstate);
                    Ok(RefreshKind::Refreshed)
                }
                None => Ok(RefreshKind::AccountOrTokenStateChanged),
            }
        }
        _ => {
            let mut ct_lk = pstate.ct_lock();
            match ct_lk.validate_act_id(act_id) {
                Some(act_id) => {
                    ct_lk.tokenstate_replace(act_id, TokenState::Empty);
                    Ok(RefreshKind::PermanentError(
                        "Received JSON in unexpected format".to_string(),
                    ))
                }
                None => Ok(RefreshKind::AccountOrTokenStateChanged),
            }
        }
    }
}

/// If `act_id` has an active token, return the time when that token should be refreshed.
fn refresh_at(
    _pstate: &AuthenticatorState,
    ct_lk: &CTGuard,
    act_id: &CTGuardAccountId,
) -> Option<Instant> {
    match ct_lk.tokenstate(act_id) {
        TokenState::Active {
            mut expiry,
            refreshed_at,
            ..
        } => {
            let act = &ct_lk.account(act_id);
            if let Some(d) = act.refresh_before_expiry {
                expiry = expiry
                    .checked_sub(d)
                    .unwrap_or_else(|| cmp::min(Instant::now(), expiry));
            }
            if let Some(d) = act.refresh_at_least {
                // There is no concept of Instant::MAX, so if `refreshed_at + d` exceeds
                // Instant's bounds, there's nothing we can fall back on.
                if let Some(t) = refreshed_at.checked_add(d) {
                    expiry = cmp::min(expiry, t);
                }
            }
            Some(expiry.to_owned())
        }
        _ => None,
    }
}

fn next_wakeup(pstate: &AuthenticatorState) -> Option<Instant> {
    let ct_lk = pstate.ct_lock();
    ct_lk
        .act_ids()
        .filter_map(|act_id| refresh_at(pstate, &ct_lk, &act_id))
        .min()
}

pub fn update_refresher(pstate: Arc<AuthenticatorState>) {
    let mut refresh_lk = pstate.refresher.pred.lock().unwrap();
    *refresh_lk = true;
    pstate.refresher.condvar.notify_one();
}

pub fn refresher_setup() -> Result<Refresher, Box<dyn Error>> {
    Ok(Refresher {
        pred: Mutex::new(false),
        condvar: Condvar::new(),
    })
}

pub fn refresher(pstate: Arc<AuthenticatorState>) -> Result<(), Box<dyn Error>> {
    thread::spawn(move || loop {
        let next_wakeup = next_wakeup(&pstate);
        let mut refresh_lk = pstate.refresher.pred.lock().unwrap();
        while !*refresh_lk {
            #[cfg(debug_assertions)]
            debug!(
                "Refresher: next wakeup {}",
                next_wakeup
                    .map(|x| x
                        .checked_duration_since(Instant::now())
                        .map(|x| x.as_secs().to_string())
                        .unwrap_or_else(|| "<none>".to_owned()))
                    .unwrap_or_else(|| "<none>".to_owned())
            );
            match next_wakeup {
                Some(t) => {
                    if Instant::now() >= t {
                        break;
                    }
                    match t.checked_duration_since(Instant::now()) {
                        Some(d) => {
                            refresh_lk = pstate
                                .refresher
                                .condvar
                                .wait_timeout(refresh_lk, d)
                                .unwrap()
                                .0
                        }
                        None => break,
                    }
                }
                None => refresh_lk = pstate.refresher.condvar.wait(refresh_lk).unwrap(),
            }
        }

        *refresh_lk = false;
        drop(refresh_lk);

        let mut ct_lk = pstate.ct_lock();
        let now = Instant::now();
        let to_refresh = ct_lk
            .act_ids()
            .filter(|act_id| refresh_at(&pstate, &ct_lk, act_id) <= Some(now))
            .collect::<Vec<_>>();

        for act_id in to_refresh.into_iter() {
            if let Some(act_id) = ct_lk.validate_act_id(act_id) {
                match refresh(Arc::clone(&pstate), ct_lk, act_id) {
                    Ok(rk) => match rk {
                        RefreshKind::AccountOrTokenStateChanged
                        | RefreshKind::Refreshed
                        | RefreshKind::TransitoryError(_) => (),
                        RefreshKind::PermanentError(msg) => {
                            error!("Token refresh failed: {msg:}")
                        }
                    },
                    Err(e) => error!("Token refresh failed: {e:}"),
                }
            }
            ct_lk = pstate.ct_lock();
        }
    });

    Ok(())
}
