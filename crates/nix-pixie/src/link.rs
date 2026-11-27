//! The controller's side of a session with a paired target: requests on
//! the control stream, and the connection that store streams go on.
//!
//! Once the target is approved, the session survives the connection
//! breaking. The link dials the target again and resumes the session (see
//! `nix_pixie_proto::ctrl`): right away if a request notices, and in the
//! background otherwise, so a target never waits on a controller that is
//! busy building. A request is never carried out twice, and one that never
//! arrived is sent again.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use iroh::endpoint::{Connection, ConnectionError, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use nix_pixie_proto::ALPN_RESUME;
use nix_pixie_proto::ctrl::{ControllerMsg, MAX_FACTER_FRAME, MAX_TARGET_FRAME, TargetMsg};
use nix_pixie_proto::frame::{read_frame, write_frame};
use nix_pixie_proto::inventory::Inventory;
use tokio::sync::{Mutex, MutexGuard, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::discovery::{self, Peer};
use crate::net::{self, Stream, sanitise};

/// How long the controller tries to get a lost connection back. The
/// target waits about as long.
pub const RECONNECT_WAIT: Duration = Duration::from_mins(10);
/// The longest pause between two attempts to reconnect.
const MAX_PAUSE: Duration = Duration::from_secs(10);

/// The controller's side of a session, from pairing on. Requests take
/// turns; a store stream may run alongside them. Dropping the link stops
/// reconnecting; [`Link::close`] ends the session.
pub struct Link {
    shared: Arc<Shared>,
    keeper: tokio::task::JoinHandle<()>,
}

/// What the link's users and its background task share.
struct Shared {
    endpoint: Endpoint,
    /// Where the target was found, to dial again.
    peer: Peer,
    /// The interfaces its link-local addresses are on, by index when it
    /// paired: to find them again if one comes back under a new index.
    interfaces: BTreeMap<u32, String>,
    state: Mutex<State>,
    /// The connection in use, without waiting for a request to finish.
    current: watch::Sender<Connection>,
    /// A connection to the target's bulk endpoint, for store copies (see
    /// [`crate::bulk`]).
    bulk: std::sync::Mutex<Option<Connection>>,
}

/// The session as the controller knows it. Locked for a whole request,
/// including getting the connection back meanwhile.
struct State {
    conn: Connection,
    stream: Stream,
    /// Final replies received so far.
    answered: u64,
    /// Whether the target was approved: only then does it take us back.
    resumable: bool,
    /// Why the session is over for good.
    gone: Option<String>,
}

/// What the target lacks after resuming.
#[derive(Debug, PartialEq, Eq)]
enum Missed {
    /// Nothing, or the request the controller waits for.
    Request,
    /// The reply to it, which follows.
    Reply,
}

/// Why an attempt to resume failed.
enum Failure {
    /// The target may be reachable later.
    Again(anyhow::Error),
    /// It won't take us back.
    Final(anyhow::Error),
}

impl Link {
    /// The link for a connection and control stream that just paired.
    pub fn new(endpoint: &Endpoint, peer: &Peer, conn: Connection, stream: Stream) -> Self {
        let shared = Arc::new(Shared {
            endpoint: endpoint.clone(),
            peer: peer.clone(),
            interfaces: discovery::interface_names(
                peer.addrs.iter().copied(),
                Path::new(discovery::SYS_NET),
            ),
            current: watch::Sender::new(conn.clone()),
            bulk: std::sync::Mutex::new(None),
            state: Mutex::new(State {
                conn,
                stream,
                answered: 0,
                resumable: false,
                gone: None,
            }),
        });
        let keeper = tokio::spawn(keep(shared.clone()));
        Self { shared, keeper }
    }

    /// The target's endpoint id.
    pub fn target(&self) -> EndpointId {
        self.shared.peer.id
    }

    /// The connection in use now: for store streams, and to see the route.
    pub fn connection(&self) -> Connection {
        self.shared.current.borrow().clone()
    }

    /// The controller's endpoint, for connections of its own to the target.
    pub fn endpoint(&self) -> &Endpoint {
        &self.shared.endpoint
    }

    /// Copies go over `conn`, to the target's bulk endpoint, from now on.
    pub fn set_bulk(&self, conn: Connection) {
        if let Ok(mut bulk) = self.shared.bulk.lock()
            && let Some(old) = bulk.replace(conn)
        {
            old.close(VarInt::from_u32(0), b"replaced");
        }
    }

    /// Runs `op` with the bulk connection if there is one, and with the
    /// session's ([`Self::on_connection`]) if not, or once it broke.
    pub async fn on_bulk_connection<T>(
        &self,
        mut op: impl AsyncFnMut(&Connection) -> Result<T>,
    ) -> Result<T> {
        let bulk = self
            .shared
            .bulk
            .lock()
            .ok()
            .and_then(|bulk| bulk.clone())
            .filter(|conn| conn.close_reason().is_none());
        if let Some(conn) = bulk {
            match op(&conn).await {
                Ok(done) => return Ok(done),
                Err(e) if conn.close_reason().is_some() => {
                    warn!("the bulk path to the target broke ({e:#}); copying over the session's");
                    if let Ok(mut bulk) = self.shared.bulk.lock() {
                        *bulk = None;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        self.on_connection(op).await
    }

    /// Reads the target's hello and inventory, the first two messages after
    /// pairing.
    pub async fn introduction(&self) -> Result<(String, Inventory)> {
        let mut state = self.lock().await?;
        crate::controller::introduction(&mut state.stream).await
    }

    /// Sends a message without a reply: `Approve` or `Reject`. After
    /// `Approve`, the target takes the link back if the connection breaks.
    pub async fn tell(&self, message: &ControllerMsg) -> Result<()> {
        let mut state = self.lock().await?;
        write_frame(&mut state.stream, message).await?;
        if matches!(message, ControllerMsg::Approve { .. }) {
            state.resumable = true;
        }
        Ok(())
    }

    /// Sends a request and waits up to `limit` for its reply. `what` is
    /// what is waited for, for the error: "the target's host key".
    pub async fn request(
        &self,
        message: &ControllerMsg,
        limit: Duration,
        what: &str,
    ) -> Result<TargetMsg> {
        self.exchange(message, Some((limit, what)), async |_| {})
            .await
    }

    /// Sends a request whose reply follows output frames (`Output`,
    /// `ExecOutput`), which go to `output` as they arrive. No time limit:
    /// the target runs something that takes as long as it takes.
    pub async fn request_with_output(
        &self,
        message: &ControllerMsg,
        output: impl AsyncFnMut(TargetMsg),
    ) -> Result<TargetMsg> {
        self.exchange(message, None, output).await
    }

    async fn exchange(
        &self,
        message: &ControllerMsg,
        limit: Option<(Duration, &str)>,
        mut output: impl AsyncFnMut(TargetMsg),
    ) -> Result<TargetMsg> {
        let mut state = self.lock().await?;
        let max = if matches!(message, ControllerMsg::RequestFacterReport) {
            MAX_FACTER_FRAME
        } else {
            MAX_TARGET_FRAME
        };
        let mut send = true;
        loop {
            let attempt = attempt(&mut state.stream, send.then_some(message), max, &mut output);
            let result = match limit {
                Some((limit, what)) => {
                    if let Ok(result) = tokio::time::timeout(limit, attempt).await {
                        result
                    } else {
                        // Its reply may still come, and would answer the
                        // next request.
                        state.gone = Some(format!("waiting for {what} took too long"));
                        bail!("timed out waiting for {what}");
                    }
                }
                None => attempt.await,
            };
            match result {
                Ok(reply) => {
                    state.answered += 1;
                    return Ok(reply);
                }
                Err(e) if state.resumable && is_lost(&state.conn) => {
                    send = self.shared.resume(&mut state, &e, true).await? == Missed::Request;
                }
                Err(e) => {
                    return Err(match closed_by_target(&state.conn) {
                        Some(reason) => anyhow!("the target ended the session: {reason}"),
                        None => e,
                    });
                }
            }
        }
    }

    /// Runs `op` with the connection, which opens its own streams on it.
    /// If the connection is lost meanwhile, runs it again once it is back:
    /// `op` must be fine with that, as copying into a store is.
    pub async fn on_connection<T>(
        &self,
        mut op: impl AsyncFnMut(&Connection) -> Result<T>,
    ) -> Result<T> {
        loop {
            let conn = self.connection();
            let error = match op(&conn).await {
                Ok(done) => return Ok(done),
                Err(e) => e,
            };
            if !is_lost(&conn) {
                return Err(error);
            }
            let mut state = self.lock().await?;
            if !state.resumable {
                return Err(error);
            }
            if state.conn.stable_id() == conn.stable_id() {
                self.shared.resume(&mut state, &error, false).await?;
            }
            info!("trying again on the new connection");
        }
    }

    /// Ends the session: the target shuts down if it was asked to.
    pub async fn close(self) {
        self.keeper.abort();
        if let Some(bulk) = self.shared.bulk.lock().ok().and_then(|mut b| b.take()) {
            bulk.close(VarInt::from_u32(0), b"done");
        }
        let mut state = self.shared.state.lock().await;
        // Closing right away would drop what is still in flight.
        net::finish(&mut state.stream).await;
        state.conn.close(VarInt::from_u32(0), b"done");
    }

    async fn lock(&self) -> Result<MutexGuard<'_, State>> {
        let state = self.shared.state.lock().await;
        if let Some(reason) = &state.gone {
            bail!("the session with the target is over: {reason}");
        }
        Ok(state)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.keeper.abort();
    }
}

/// Sends `message` (unless `None`: resending isn't needed) and reads frames
/// of at most `max` bytes until the reply, handing others to `output`.
async fn attempt(
    stream: &mut Stream,
    message: Option<&ControllerMsg>,
    max: usize,
    output: &mut impl AsyncFnMut(TargetMsg),
) -> Result<TargetMsg> {
    if let Some(message) = message {
        write_frame(stream, message).await?;
    }
    loop {
        match read_frame(stream, max).await? {
            frame @ (TargetMsg::Output { .. } | TargetMsg::ExecOutput { .. }) => {
                output(frame).await;
            }
            reply => return Ok(reply),
        }
    }
}

impl Shared {
    /// Gets the connection back and resumes the session, trying for up to
    /// [`RECONNECT_WAIT`]. `pending`: a request waits for its reply. If
    /// that fails, the session is over.
    async fn resume(
        &self,
        state: &mut State,
        cause: &anyhow::Error,
        pending: bool,
    ) -> Result<Missed> {
        warn!(
            target = %self.peer.id,
            "lost the connection to the target ({cause:#}); reconnecting for up to {} minutes",
            RECONNECT_WAIT.as_secs() / 60
        );
        let deadline = Instant::now() + RECONNECT_WAIT;
        let mut pause = Duration::from_secs(1);
        let mut attempts = 0_u32;
        loop {
            attempts += 1;
            let error = match self.reconnect(state, pending).await {
                Ok(missed) => {
                    info!(
                        target = %self.peer.id,
                        attempts,
                        route = %net::route(&state.conn),
                        "reconnected; the session goes on"
                    );
                    return Ok(missed);
                }
                Err(Failure::Final(e)) => e,
                Err(Failure::Again(e)) if Instant::now() + pause < deadline => {
                    debug!(attempts, "reconnecting: {e:#}");
                    tokio::time::sleep(pause).await;
                    pause = pause.saturating_mul(2).min(MAX_PAUSE);
                    continue;
                }
                Err(Failure::Again(e)) => e.context(format!(
                    "no connection to the target for {} minutes",
                    RECONNECT_WAIT.as_secs() / 60
                )),
            };
            state.gone = Some(format!("{error:#}"));
            return Err(error.context("lost the connection to the target"));
        }
    }

    /// One attempt: dial the target, and resume where both left off.
    async fn reconnect(&self, state: &mut State, pending: bool) -> Result<Missed, Failure> {
        let mut routes = net::routes(&self.peer);
        // It may have been found at an address it was not announced at.
        for path in state.conn.paths().iter() {
            if !routes.contains(path.remote_addr()) {
                routes.push(path.remote_addr().clone());
            }
        }
        // A USB link the host enumerated anew is a new interface, with a new
        // index: its link-local addresses are only reachable under that.
        let ips: Vec<_> = routes
            .iter()
            .filter_map(|route| match route {
                TransportAddr::Ip(addr) => Some(*addr),
                _ => None,
            })
            .collect();
        for addr in discovery::rescoped(&ips, &self.interfaces, Path::new(discovery::SYS_NET)) {
            debug!(%addr, "the interface came back under another index");
            routes.push(TransportAddr::Ip(addr));
        }
        let addr = EndpointAddr::from_parts(self.peer.id, routes);
        let conn = tokio::time::timeout(
            net::CONNECT_TIMEOUT,
            self.endpoint.connect(addr, ALPN_RESUME),
        )
        .await
        .map_err(|_| Failure::Again(anyhow!("timed out connecting")))?
        .map_err(|e| Failure::Again(e.into()))?;
        let handshake = async {
            let (send, recv) = conn.open_bi().await?;
            let mut stream = tokio::io::join(recv, send);
            let resume = ControllerMsg::Resume {
                answered: state.answered,
            };
            write_frame(&mut stream, &resume).await?;
            // No time limit: the target first finishes what it was doing.
            let reply: TargetMsg = read_frame(&mut stream, MAX_TARGET_FRAME).await?;
            anyhow::Ok((stream, reply))
        };
        let (stream, reply) = handshake.await.map_err(|e| match conn.close_reason() {
            Some(ConnectionError::ApplicationClosed(close)) => Failure::Final(anyhow!(
                "the target turned us away: {}",
                sanitise(&String::from_utf8_lossy(&close.reason), 100)
            )),
            _ => Failure::Again(e),
        })?;
        let TargetMsg::Resumed { answered } = reply else {
            return Err(Failure::Final(anyhow!(
                "the target didn't resume the session"
            )));
        };
        let missed = match answered.checked_sub(state.answered) {
            Some(0) => Missed::Request,
            Some(1) if pending => Missed::Reply,
            _ => {
                return Err(Failure::Final(anyhow!(
                    "the target answered {answered} requests, but {} replies arrived",
                    state.answered
                )));
            }
        };
        state.conn = conn.clone();
        state.stream = stream;
        self.current.send_replace(conn);
        Ok(missed)
    }
}

/// Why the target closed `conn`, if it did on purpose.
fn closed_by_target(conn: &Connection) -> Option<String> {
    match conn.close_reason()? {
        ConnectionError::ApplicationClosed(close) => {
            Some(sanitise(&String::from_utf8_lossy(&close.reason), 200))
        }
        _ => None,
    }
}

/// Whether `conn` broke: neither side closed it on purpose.
fn is_lost(conn: &Connection) -> bool {
    conn.close_reason().is_some_and(|reason| {
        !matches!(
            reason,
            ConnectionError::ApplicationClosed(_) | ConnectionError::LocallyClosed
        )
    })
}

/// Gets the connection back as soon as it breaks, if no request does.
async fn keep(shared: Arc<Shared>) {
    let mut current = shared.current.subscribe();
    loop {
        let conn = current.borrow_and_update().clone();
        tokio::select! {
            _ = conn.closed() => {}
            changed = current.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
        }
        let mut state = shared.state.lock().await;
        if state.conn.stable_id() != conn.stable_id() {
            // A request got it back already.
            continue;
        }
        if !state.resumable || state.gone.is_some() || !is_lost(&conn) {
            return;
        }
        let cause = anyhow!(
            "{}",
            conn.close_reason()
                .map_or_else(|| "closed".to_string(), |reason| reason.to_string())
        );
        if shared.resume(&mut state, &cause, false).await.is_err() {
            return;
        }
    }
}
