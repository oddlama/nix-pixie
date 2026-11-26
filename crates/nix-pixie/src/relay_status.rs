//! Whether the agent's relay is reachable, for the operator at the target:
//! without it, only controllers on the same network find the machine.
//!
//! Printed when it changes, and part of every pairing code shown. A relay
//! counts as unreachable only after [`PATIENCE`] without it, so a network
//! that is still coming up (DHCP) or a relay that restarts doesn't alarm
//! anybody.

use std::time::Duration;

use iroh::{Endpoint, Watcher};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::screen;

/// How long the relay may be away before it counts as unreachable.
pub const PATIENCE: Duration = Duration::from_secs(15);
/// The longest error shown.
const MAX_ERROR: usize = 200;

/// One home relay as iroh reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub url: String,
    pub connected: bool,
    /// The last failed attempt to connect, if any.
    pub error: Option<String>,
}

/// What the operator is told about the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Not connected yet, within [`PATIENCE`].
    Trying,
    /// Connected to this relay.
    Reachable(String),
    /// Away for longer than [`PATIENCE`]: the relay (if one was picked), and
    /// why, as far as known.
    Unreachable {
        relay: Option<String>,
        error: Option<String>,
    },
}

impl Reach {
    /// The state from iroh's `samples`, where `away` is how long no relay
    /// has been connected (since the start, or since it was lost).
    #[must_use]
    pub fn judge(samples: &[Sample], away: Duration) -> Self {
        if let Some(sample) = samples.iter().find(|s| s.connected) {
            return Self::Reachable(sample.url.clone());
        }
        if away < PATIENCE {
            return Self::Trying;
        }
        let failed = samples.iter().find(|s| s.error.is_some());
        Self::Unreachable {
            relay: failed.or(samples.first()).map(|s| s.url.clone()),
            error: failed.and_then(|s| s.error.clone()),
        }
    }

    /// The same as `other` for the operator: only a change of kind or relay
    /// is news, not another error from the same relay.
    fn same_news(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Trying, Self::Trying) => true,
            (Self::Reachable(a), Self::Reachable(b)) => a == b,
            (Self::Unreachable { relay: a, .. }, Self::Unreachable { relay: b, .. }) => a == b,
            _ => false,
        }
    }

    /// One line for the target's screen. `configured`: the relays the
    /// agent uses, for when it hasn't picked one yet.
    #[must_use]
    pub fn line(&self, configured: &str) -> String {
        match self {
            Self::Trying => format!("Relay: trying to reach {configured}..."),
            Self::Reachable(relay) => format!(
                "Relay: {relay} is reachable, so controllers on other networks can find this \
                 machine."
            ),
            Self::Unreachable { relay, error } => {
                let why = error
                    .as_deref()
                    .map_or_else(String::new, |e| format!(" ({e})"));
                let what = relay.as_deref().map_or_else(
                    || format!("can't reach {configured}"),
                    |relay| format!("{relay} is not reachable"),
                );
                format!(
                    "Relay: {what}{why}. Until it is, only controllers on this network can find \
                     this machine."
                )
            }
        }
    }
}

/// Follows the relay status of `endpoint` for as long as it runs: prints
/// every change and keeps `shown` up to date.
pub async fn watch(endpoint: Endpoint, configured: String, shown: watch::Sender<Reach>) {
    let mut statuses = endpoint.home_relay_status();
    let mut current = statuses.get();
    let mut away_since = Instant::now();
    loop {
        let samples: Vec<Sample> = current
            .iter()
            .map(|status| Sample {
                url: status.url().to_string(),
                connected: status.is_connected(),
                error: status.last_error().map(|e| {
                    let text = format!("{e:#}");
                    text.chars().take(MAX_ERROR).collect()
                }),
            })
            .collect();
        if samples.iter().any(|s| s.connected) {
            away_since = Instant::now();
        }
        let reach = Reach::judge(&samples, away_since.elapsed());
        if !reach.same_news(&shown.borrow()) {
            report(&reach, &configured);
        }
        screen::relay(Some(reach.line(&configured)));
        shown.send_replace(reach.clone());
        // Wake up when it changes, or when the patience runs out.
        let deadline = (!matches!(reach, Reach::Unreachable { .. } | Reach::Reachable(_)))
            .then(|| away_since + PATIENCE);
        tokio::select! {
            updated = statuses.updated() => match updated {
                Ok(value) => current = value,
                Err(_) => return,
            },
            () = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
}

fn report(reach: &Reach, configured: &str) {
    match reach {
        Reach::Trying => return,
        Reach::Reachable(relay) => info!(%relay, "relay reachable"),
        Reach::Unreachable { relay, error } => warn!(?relay, ?error, "relay not reachable"),
    }
    screen::say(reach.line(configured));
}
