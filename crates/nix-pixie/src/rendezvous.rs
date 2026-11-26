//! Meeting at a relay: how a controller on another network finds the target
//! showing a code. `nix_pixie_proto::rendezvous` explains the idea; this is
//! both ends of it over iroh.
//!
//! The target's side is a second endpoint next to the agent's own, bound
//! only while a code is shown. It has no sockets, only relay connections,
//! and tells whoever connects the agent's announcement. The controller's
//! side connects to it again and again until something answers.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::endpoint::{Connection, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey, TransportAddr, Watcher};
use nix_pixie_proto::ALPN_RENDEZVOUS;
use nix_pixie_proto::announce::{Announcement, Pairing};
use nix_pixie_proto::rendezvous::{MAX_REPLY, decode, encode, secret};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

use crate::dns;
use crate::net::Relays;
use crate::state::StateDir;

/// For one question at the rendezvous: connecting to the relays, the
/// handshake through one of them, and the answer.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);
/// Pause after a question nobody answered.
const RETRY_INTERVAL: Duration = Duration::from_millis(1500);
/// Pause after an answer, before asking again in case somebody else
/// answers now (two targets that drew the same nameplate take turns).
const RECHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How long an asker gets to take its answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Askers served at once. The rest wait their turn.
const MAX_ASKERS: usize = 16;
/// How long leaving the rendezvous may hold up what comes next.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for a relay before saying that there is none.
const RELAY_PATIENCE: Duration = Duration::from_secs(15);

/// The endpoint id everybody derives from `nameplate`.
pub fn endpoint_id(nameplate: u32) -> EndpointId {
    key(nameplate).public()
}

fn key(nameplate: u32) -> SecretKey {
    SecretKey::from_bytes(&secret(nameplate))
}

/// The relay `endpoint` is connected to and reachable through, if any.
pub fn home_relay(endpoint: &Endpoint) -> Option<RelayUrl> {
    endpoint
        .home_relay_status()
        .get()
        .iter()
        .find(|relay| relay.is_connected())
        .map(|relay| relay.url().clone())
}

/// Why `endpoint` has no relay, as far as it knows.
pub fn relay_error(endpoint: &Endpoint) -> Option<String> {
    endpoint
        .home_relay_status()
        .get()
        .iter()
        .find_map(|relay| Some(format!("{}: {:#}", relay.url(), relay.last_error()?)))
}

/// A target waiting at its rendezvous until [`Self::close`].
pub struct Rendezvous {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Rendezvous {
    /// Starts answering with `announcement` at the rendezvous of its
    /// nameplate, once `agent` (the endpoint it announces) is reachable
    /// through a relay itself. Writes that relay to the `relay` state file.
    /// `None` if the announcement has no nameplate to meet at.
    pub fn spawn(
        agent: Endpoint,
        announcement: Announcement,
        relays: Relays,
        state: StateDir,
    ) -> Option<Self> {
        let Pairing::ShowsCode { nameplate } = announcement.pairing else {
            return None;
        };
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut endpoint = None;
            tokio::select! {
                result = wait(&agent, nameplate, &announcement, &relays, &state, &mut endpoint) => {
                    if let Err(e) = result {
                        warn!("rendezvous failed: {e:#}");
                    }
                }
                _ = stopped => {}
            }
            state.remove("relay");
            // Closing tells the relay we left, so the rendezvous isn't
            // occupied until the connection times out.
            if let Some(endpoint) = endpoint {
                endpoint.close().await;
            }
        });
        Some(Self { stop, task })
    }

    /// Leaves the rendezvous.
    pub async fn close(self) {
        let _ = self.stop.send(());
        if tokio::time::timeout(CLOSE_TIMEOUT, self.task)
            .await
            .is_err()
        {
            debug!("rendezvous endpoint did not close in time");
        }
    }
}

/// Serves the rendezvous; never returns unless it fails. The endpoint goes
/// into `bound` as soon as there is one, for the caller to close.
async fn wait(
    agent: &Endpoint,
    nameplate: u32,
    announcement: &Announcement,
    relays: &Relays,
    state: &StateDir,
    bound: &mut Option<Endpoint>,
) -> Result<()> {
    let reply = encode(announcement).context("encoding the announcement")?;
    // Without a relay of its own the agent can't be dialed through one, so
    // sending controllers to it would only make them wait.
    if tokio::time::timeout(RELAY_PATIENCE, agent.online())
        .await
        .is_err()
    {
        // The operator hears of it from `relay_status`.
        debug!(
            "no relay reachable{}; not at the rendezvous until one is",
            relay_error(agent).map_or_else(String::new, |e| format!(" ({e})"))
        );
        agent.online().await;
    }

    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(key(nameplate))
        .alpns(vec![ALPN_RENDEZVOUS.to_vec()])
        .relay_mode(relays.mode())
        .dns_resolver(dns::resolver())
        .clear_ip_transports()
        .bind()
        .await
        .context("binding the rendezvous endpoint")?;
    *bound = Some(endpoint.clone());
    endpoint.online().await;
    let relay = home_relay(agent).map_or_else(|| "a relay".to_string(), |url| url.to_string());
    info!(%relay, "waiting at the rendezvous");
    state.write("relay", &relay)?;

    let mut askers = JoinSet::new();
    loop {
        while askers.try_join_next().is_some() {}
        while askers.len() >= MAX_ASKERS {
            askers.join_next().await;
        }
        let incoming = endpoint.accept().await.context("endpoint closed")?;
        let reply = reply.clone();
        askers.spawn(async move {
            let answered = tokio::time::timeout(REPLY_TIMEOUT, async {
                let conn = incoming.await?;
                answer(&conn, &reply).await
            })
            .await;
            match answered {
                Ok(Ok(asker)) => debug!(%asker, "answered at the rendezvous"),
                Ok(Err(e)) => debug!("asker at the rendezvous failed: {e:#}"),
                Err(_) => debug!("asker at the rendezvous timed out"),
            }
        });
    }
}

/// Sends `reply` on a stream of its own and waits until the asker has it.
async fn answer(conn: &Connection, reply: &[u8]) -> Result<EndpointId> {
    let mut send = conn.open_uni().await?;
    send.write_all(reply).await?;
    send.finish()?;
    // The asker closes once it has read everything.
    conn.closed().await;
    Ok(conn.remote_id())
}

/// Asks at the rendezvous of `nameplate` who is waiting there, through any
/// of `relays`. The answer is unauthenticated, like every announcement.
pub async fn lookup(
    endpoint: &Endpoint,
    nameplate: u32,
    relays: &BTreeSet<RelayUrl>,
) -> Result<Announcement> {
    let addr = EndpointAddr::from_parts(
        endpoint_id(nameplate),
        relays.iter().cloned().map(TransportAddr::Relay),
    );
    tokio::time::timeout(LOOKUP_TIMEOUT, async {
        let conn = endpoint.connect(addr, ALPN_RENDEZVOUS).await?;
        let reply = async {
            let mut recv = conn.accept_uni().await?;
            anyhow::Ok(recv.read_to_end(MAX_REPLY).await?)
        }
        .await;
        conn.close(VarInt::from_u32(0), b"thanks");
        anyhow::Ok(decode(&reply?)?)
    })
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("nobody answered")))
}

/// What a [`Search`] has seen so far, for error messages.
#[derive(Debug, Clone, Default)]
pub struct SearchStatus {
    /// Why the last question got no answer.
    pub last_error: Option<String>,
}

/// The controller asking at a rendezvous until somebody answers, and now
/// and then afterwards.
pub struct Search {
    task: JoinHandle<()>,
    found: mpsc::Receiver<Announcement>,
    status: watch::Receiver<SearchStatus>,
    answers: Arc<AtomicUsize>,
    endpoint: Endpoint,
    /// The relays, for messages.
    name: String,
    /// Where a target that answered can be dialed.
    pub relays: BTreeSet<RelayUrl>,
}

impl Search {
    /// Starts asking for the target showing `nameplate`, through `relays`.
    pub fn spawn(endpoint: Endpoint, nameplate: u32, relays: &Relays) -> Self {
        let name = relays.to_string();
        let relays = relays.urls();
        let (tx, found) = mpsc::channel(4);
        let (status_tx, status) = watch::channel(SearchStatus::default());
        let answers = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let endpoint = endpoint.clone();
            let relays = relays.clone();
            let answers = answers.clone();
            async move {
                loop {
                    let pause = match lookup(&endpoint, nameplate, &relays).await {
                        // Whoever holds the rendezvous may announce anything;
                        // only a target showing this nameplate is of use.
                        Ok(announcement)
                            if announcement.pairing == (Pairing::ShowsCode { nameplate }) =>
                        {
                            answers.fetch_add(1, Ordering::Relaxed);
                            if tx.send(announcement).await.is_err() {
                                return;
                            }
                            RECHECK_INTERVAL
                        }
                        Ok(_) => {
                            status_tx.send_replace(SearchStatus {
                                last_error: Some("the answer was for another code".into()),
                            });
                            RETRY_INTERVAL
                        }
                        Err(e) => {
                            debug!(nameplate, "rendezvous: {e:#}");
                            status_tx.send_replace(SearchStatus {
                                last_error: Some(format!("{e:#}")),
                            });
                            RETRY_INTERVAL
                        }
                    };
                    tokio::time::sleep(pause).await;
                }
            }
        });
        Self {
            task,
            found,
            status,
            answers,
            endpoint,
            name,
            relays,
        }
    }

    /// The next answer. Pends forever once the search has stopped.
    pub async fn recv(&mut self) -> Announcement {
        match self.found.recv().await {
            Some(announcement) => announcement,
            None => std::future::pending().await,
        }
    }

    /// What the search has come to, for "nothing found" errors.
    pub fn describe(&self) -> String {
        let relays = &self.name;
        let answers = self.answers.load(Ordering::Relaxed);
        if answers > 0 {
            return format!("relay: {answers} answer(s) through {relays}");
        }
        if home_relay(&self.endpoint).is_none() {
            let error = relay_error(&self.endpoint).map_or_else(String::new, |e| format!(": {e}"));
            return format!("relay: this machine reached none of {relays}{error}");
        }
        let error = self
            .status
            .borrow()
            .last_error
            .clone()
            .map_or_else(String::new, |e| format!(": {e}"));
        format!("relay: no target waiting at {relays}{error}")
    }

    pub fn shutdown(self) {
        self.task.abort();
    }
}
