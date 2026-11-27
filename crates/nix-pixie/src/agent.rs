//! The installer-side agent: announces itself, waits for a controller to dial
//! in and pair, and (for now) holds the session open until the controller
//! leaves.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, ConnectionError, VarInt};
use iroh::{Endpoint, EndpointId, SecretKey};
use mdns_sd::ServiceEvent;
use nix_pixie_proto::announce::{Announcement, Pairing};
use nix_pixie_proto::code::{Code, NameplateSpace};
use nix_pixie_proto::ctrl::{
    ControllerMsg, DiskAssignment, KeyFile, MAX_CONTROLLER_FRAME, MAX_STREAM_REQUEST, Secret,
    Shutdown, StreamRequest, TargetMsg,
};
use nix_pixie_proto::frame::{FrameError, read_frame, write_frame};
use nix_pixie_proto::pair::Side;
use nix_pixie_proto::{ALPN_CTRL, ALPN_RESUME};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::child;
use crate::control;
use crate::disko;
use crate::exec;
use crate::inventory;
use crate::mdns::{self, Mdns};
use crate::net::{self, Relays, Stream};
use crate::nixos;
use crate::probe::Responder;
use crate::relay_status::{self, Reach};
use crate::rendezvous::Rendezvous;
use crate::screen::{self, Shown};
use crate::state::StateDir;
use crate::store;

/// Spent guesses before a code is given up: the owner shows a new one, the
/// typist asks for it to be entered again. Each guess has ~1 in 2^31 odds.
pub const MAX_FAILURES: u32 = 3;
/// How long to wait for the controller to leave after `Finished` before
/// shutting down anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Pause after the first rotation before showing a new code; doubles with
/// each further rotation, up to [`MAX_ROTATION_DELAY`].
const ROTATION_DELAY: Duration = Duration::from_secs(2);
/// The longest pause between codes. Bounds online guessing to
/// [`MAX_FAILURES`] guesses per minute: an agent on the internet can be
/// dialled by anyone, not just by machines on its own network.
const MAX_ROTATION_DELAY: Duration = Duration::from_secs(60);
/// How long the agent waits for its port, which the endpoint it closed last
/// may hold a moment longer.
const REBIND_WAIT: Duration = Duration::from_secs(10);
/// The close code for a controller that sent what we can't read.
const PROTOCOL_BROKEN: u32 = 3;
/// How long to listen for other targets' nameplates before picking ours.
const NAMEPLATE_SURVEY: Duration = Duration::from_secs(2);

/// How `nix-pixie agent` was invoked.
pub struct AgentArgs {
    pub state_dir: PathBuf,
    pub port: u16,
    pub controller_code: Option<String>,
    /// Where controllers on other networks can find this machine.
    pub relays: Relays,
}

pub async fn run(args: AgentArgs) -> Result<()> {
    // Parse a typed code before touching the network, so typos fail fast.
    let controller_code = args
        .controller_code
        .as_deref()
        .map(Code::parse)
        .transpose()?;
    if let Some(code) = controller_code
        && code.nameplate().is_some()
    {
        bail!(
            "{code} starts with a number, so it is a target code; enter the code shown by the controller"
        );
    }

    let state = StateDir::create(args.state_dir)?;
    // From an earlier run of the agent during this boot: the disks it
    // formatted, the host key it made and what the operator chose carry
    // over, its session doesn't.
    for stale in ["code", "paired-controller", "approved", "relay"] {
        state.remove(stale);
    }
    let started_with = controller_code;
    let mut code = given_earlier(&state).or(started_with);
    let configured = args.relays;
    let mut relays = chosen_earlier(&state).unwrap_or_else(|| configured.clone());
    // A fresh identity per boot, the same for every endpoint of this run.
    // Nothing persists on the installer.
    let secret = SecretKey::generate();
    let mdns = Mdns::new()?;
    let responder = Responder::bind().await?;
    let (announce_tx, announce_rx) = watch::channel(None);
    let responder = tokio::spawn(responder.run(announce_rx));
    let (changes_tx, mut changes) = mpsc::channel(1);
    let idle = Arc::new(AtomicBool::new(false));
    let control = tokio::spawn(control::serve(
        state.path().join(control::SOCKET),
        Arc::new(control::Agent {
            configured,
            changes: changes_tx,
            idle: idle.clone(),
        }),
    ));
    let mut requests = Requests {
        changes: &mut changes,
        idle: &idle,
    };
    let shared = Shared {
        secret: &secret,
        port: args.port,
        mdns: &mdns,
        probe: &announce_tx,
        state: &state,
    };
    let result = loop {
        // A controller's code has no nameplate to meet at: without one,
        // relays are of no use.
        let using = if code.is_some() {
            Relays::Disabled
        } else {
            relays.clone()
        };
        match listen(&shared, &using, code.as_ref(), &mut requests).await {
            Ok(Outcome::Change(change)) => match change.wanted {
                control::Wanted::Relays {
                    relays: wanted,
                    chosen,
                } => relays = take_relays(&state, wanted, chosen.as_deref(), change.answer),
                control::Wanted::Code(wanted) => {
                    code = take_code(&state, wanted, started_with, change.answer);
                }
            },
            Ok(Outcome::Done) => break Ok(()),
            // Given at the console: forgotten, so it can't be guessed at
            // any longer. Ours otherwise, from the command line: the
            // service manager starts us again.
            Ok(Outcome::Rejected) if state.read(control::GIVEN_CODE).is_some() => {
                state.remove(control::GIVEN_CODE);
                code = started_with;
                screen::say(format!(
                    "The controller's code was rejected {MAX_FAILURES} times; give it again \
                     (`code CODE`)."
                ));
            }
            Ok(Outcome::Rejected) => {
                break Err(anyhow::anyhow!(
                    "the code was rejected {MAX_FAILURES} times; check the code shown by the \
                     controller and enter it again"
                ));
            }
            Err(e) => break Err(e),
        }
    };
    control.abort();
    responder.abort();
    mdns.shutdown();
    result
}

/// What every endpoint of the agent's run shares.
struct Shared<'a> {
    secret: &'a SecretKey,
    port: u16,
    mdns: &'a Mdns,
    probe: &'a watch::Sender<Option<Announcement>>,
    state: &'a StateDir,
}

/// The controller's code given with `nix-pixie code` earlier during this
/// boot.
fn given_earlier(state: &StateDir) -> Option<Code> {
    let text = state.read(control::GIVEN_CODE)?;
    match control::controller_code(text.trim()) {
        Ok(code) => {
            info!("waiting for the controller whose code was given earlier during this boot");
            Some(code)
        }
        Err(e) => {
            warn!("ignoring the controller's code given earlier: {e}");
            state.remove(control::GIVEN_CODE);
            None
        }
    }
}

/// The relays chosen with `nix-pixie relay` earlier during this boot.
fn chosen_earlier(state: &StateDir) -> Option<Relays> {
    let text = state.read(control::CHOSEN)?;
    match control::chosen(&text) {
        Ok(relays) => {
            info!(%relays, "using the relays chosen earlier during this boot");
            Some(relays)
        }
        Err(e) => {
            warn!("ignoring the relays chosen earlier: {e:#}");
            None
        }
    }
}

/// Waits for controllers on an endpoint with `relays`, until shut down or
/// the operator chooses other relays.
async fn listen(
    shared: &Shared<'_>,
    relays: &Relays,
    controller_code: Option<&Code>,
    requests: &mut Requests<'_>,
) -> Result<Outcome> {
    let endpoint = bind(shared, relays).await?;
    shared
        .state
        .write("endpoint-id", &endpoint.id().to_string())?;
    info!(id = %endpoint.id(), port = net::bound_port(&endpoint)?, %relays, "agent listening");
    let (reach, watching) = if relays.is_enabled() {
        let (shown, reach) = watch::channel(Reach::Trying);
        let task = tokio::spawn(relay_status::watch(
            endpoint.clone(),
            relays.to_string(),
            shown,
        ));
        (Some(reach), Some(task))
    } else {
        (None, None)
    };
    let mut announcer = Announcer {
        endpoint: &endpoint,
        mdns: shared.mdns,
        probe: shared.probe,
        relays,
        reach,
        state: shared.state,
        advertised: None,
        rendezvous: None,
    };
    let outcome = tokio::select! {
        r = async {
            match controller_code {
                None => show_code(&mut announcer, shared.state, requests).await,
                Some(code) => typed_code(&mut announcer, shared.state, code, requests).await,
            }
        } => r,
        () = crate::shutdown_signal() => Ok(Outcome::Done),
    };
    announcer.withdraw().await;
    if let Some(task) = watching {
        task.abort();
        // Its clone of the endpoint holds the port until it is gone.
        let _ = task.await;
    }
    screen::code(None);
    screen::relay(None);
    endpoint.close().await;
    outcome
}

/// The agent's endpoint, with `relays`. Right after the operator changed
/// something, the last one may not have let go of the port yet.
async fn bind(shared: &Shared<'_>, relays: &Relays) -> Result<Endpoint> {
    let deadline = tokio::time::Instant::now() + REBIND_WAIT;
    loop {
        match net::bind(shared.secret.clone(), shared.port, relays).await {
            Ok(endpoint) => return Ok(endpoint),
            Err(e) if in_use(&e) && tokio::time::Instant::now() < deadline => {
                debug!("port {} still in use; trying again", shared.port);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether `error` is that the address is in use.
fn in_use(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse)
    })
}

/// Keeps the relays the operator chose for the rest of the boot, and
/// answers them.
fn take_relays(
    state: &StateDir,
    relays: Relays,
    chosen: Option<&str>,
    answer: oneshot::Sender<Result<String, String>>,
) -> Relays {
    let kept = if let Some(text) = chosen {
        state.write(control::CHOSEN, text)
    } else {
        state.remove(control::CHOSEN);
        Ok(())
    };
    let reply = kept
        .map(|()| format!("now using {relays}; the target shows a new code"))
        .map_err(|e| format!("{e:#}"));
    let _ = answer.send(reply);
    screen::say(format!("Relays changed to {relays}."));
    relays
}

/// Keeps the controller's code the operator gave for the rest of the boot
/// (or forgets it, for `None`), and answers it. Returns the code to wait
/// for.
fn take_code(
    state: &StateDir,
    code: Option<Code>,
    started_with: Option<Code>,
    answer: oneshot::Sender<Result<String, String>>,
) -> Option<Code> {
    let (kept, now, reply) = if let Some(code) = code {
        (
            state.write(control::GIVEN_CODE, &code.to_string()),
            Some(code),
            "waiting for the controller with that code",
        )
    } else {
        state.remove(control::GIVEN_CODE);
        let reply = if started_with.is_some() {
            "waiting for the controller whose code the agent was started with"
        } else {
            "the target shows a code of its own again"
        };
        (Ok(()), started_with, reply)
    };
    let _ = answer.send(
        kept.map(|()| reply.to_string())
            .map_err(|e| format!("{e:#}")),
    );
    now
}

/// Why the agent stopped waiting for controllers.
enum Outcome {
    /// Shut down, or (with a controller's code) the session is over.
    Done,
    /// The operator asked for other relays or another code.
    Change(control::Change),
    /// The controller's code was rejected [`MAX_FAILURES`] times.
    Rejected,
}

/// Changes from the control socket. They are only taken while the agent
/// waits for a controller, never while one pairs or is paired.
struct Requests<'a> {
    changes: &'a mut mpsc::Receiver<control::Change>,
    /// Set while waiting, for the control socket to see.
    idle: &'a AtomicBool,
}

/// Publishes our announcement via DNS-SD and probe replies and, with
/// relays, at the rendezvous of its nameplate.
struct Announcer<'a> {
    endpoint: &'a Endpoint,
    mdns: &'a Mdns,
    probe: &'a watch::Sender<Option<Announcement>>,
    relays: &'a Relays,
    /// Whether the relay is reachable, with relays.
    reach: Option<watch::Receiver<Reach>>,
    state: &'a StateDir,
    advertised: Option<String>,
    rendezvous: Option<Rendezvous>,
}

impl Announcer<'_> {
    async fn announce(&mut self, pairing: Pairing) -> Result<()> {
        self.withdraw().await;
        let announcement = Announcement::new(
            *self.endpoint.id().as_bytes(),
            net::bound_port(self.endpoint)?,
            pairing,
            &net::hostname(),
        );
        self.advertised = Some(self.mdns.advertise(&announcement)?);
        self.probe.send_replace(Some(announcement.clone()));
        if self.relays.is_enabled() {
            self.rendezvous = Rendezvous::spawn(
                self.endpoint.clone(),
                announcement,
                self.relays.clone(),
                self.state.clone(),
            );
        }
        Ok(())
    }

    async fn withdraw(&mut self) {
        if let Some(name) = self.advertised.take() {
            self.mdns.withdraw(&name);
        }
        self.probe.send_replace(None);
        if let Some(rendezvous) = self.rendezvous.take() {
            rendezvous.close().await;
        }
    }

    /// The nameplates our codes use: long ones if controllers may have to
    /// find us at a relay, where every other target's nameplate is too.
    fn nameplate_space(&self) -> NameplateSpace {
        if self.relays.is_enabled() {
            NameplateSpace::Relay
        } else {
            NameplateSpace::Local
        }
    }

    /// How the relay stands, for the code's banner (nothing without
    /// relays).
    fn relay_line(&self) -> Option<String> {
        let reach = self.reach.as_ref()?;
        Some(reach.borrow().line(&self.relays.to_string()))
    }

    /// What the controller needs besides the code to find us.
    fn controller_flags(&self) -> String {
        match self.relays {
            Relays::Custom(urls) => urls.iter().fold(String::new(), |mut flags, url| {
                let _ = write!(flags, " --relay {url}");
                flags
            }),
            Relays::N0 | Relays::Disabled => String::new(),
        }
    }
}

/// Default mode: show a code, let the operator type it on the controller.
/// Returns when the operator asks for other relays or a controller's code.
async fn show_code(
    announcer: &mut Announcer<'_>,
    state: &StateDir,
    requests: &mut Requests<'_>,
) -> Result<Outcome> {
    let endpoint = announcer.endpoint;
    let mut rotations = 0;
    loop {
        let taken = nameplates_in_use(announcer.mdns, endpoint).await?;
        let nameplate = Code::free_nameplate(announcer.nameplate_space(), &taken)
            .context("all nameplates on this network are in use")?;
        let code = Code::generate(Some(nameplate))?;
        announcer.announce(Pairing::ShowsCode { nameplate }).await?;
        state.write("code", &code.to_string())?;
        screen::code(Some(Shown {
            code: code.to_string(),
            command: format!(
                "nix-pixie controller pair --code {code}{}",
                announcer.controller_flags()
            ),
        }));
        if let Some(line) = announcer.relay_line() {
            println!("    {line}\n");
        }

        let paired = accept_pairing(endpoint, &code, Side::OwnsCode, requests).await;
        announcer.withdraw().await;
        state.remove("code");
        screen::code(None);
        match paired? {
            Accepted::Paired(conn, stream) => {
                if let Err(e) = session(endpoint, state, &conn, stream).await {
                    warn!("session ended with an error: {e:#}");
                }
            }
            Accepted::Spent => {
                rotations += 1;
                let delay = rotation_delay(rotations);
                warn!("{MAX_FAILURES} pairing attempts with a wrong code");
                screen::say(format!(
                    "{MAX_FAILURES} pairing attempts with a wrong code; someone may be guessing. \
                     A new code follows in {} s.",
                    delay.as_secs()
                ));
                tokio::time::sleep(delay).await;
            }
            Accepted::Change(change) => return Ok(Outcome::Change(change)),
        }
    }
}

/// The pause before the new code after `rotations` rotations (counting from
/// 1): [`ROTATION_DELAY`], doubling up to [`MAX_ROTATION_DELAY`].
#[must_use]
pub fn rotation_delay(rotations: u32) -> Duration {
    let doublings = rotations.saturating_sub(1).min(16);
    ROTATION_DELAY
        .saturating_mul(1 << doublings)
        .min(MAX_ROTATION_DELAY)
}

/// Nameplates other targets currently advertise via DNS-SD.
async fn nameplates_in_use(mdns: &Mdns, endpoint: &Endpoint) -> Result<Vec<u32>> {
    let events = mdns.browse()?;
    let mut taken = Vec::new();
    let _ = tokio::time::timeout(NAMEPLATE_SURVEY, async {
        while let Ok(event) = events.recv_async().await {
            if let ServiceEvent::ServiceResolved(service) = event
                && let Ok((announcement, _)) = mdns::parse(&service)
                && announcement.id != *endpoint.id().as_bytes()
                && let Pairing::ShowsCode { nameplate } = announcement.pairing
            {
                taken.push(nameplate);
            }
        }
    })
    .await;
    Ok(taken)
}

/// Alternative mode: the operator gave the controller's code here (typed,
/// as a credential, through the console). The controller finds us and dials
/// in. Returns when the session is over or the operator asks for another
/// code.
async fn typed_code(
    announcer: &mut Announcer<'_>,
    state: &StateDir,
    code: &Code,
    requests: &mut Requests<'_>,
) -> Result<Outcome> {
    announcer.announce(Pairing::TypedControllerCode).await?;
    screen::say("Waiting for the controller whose code was given here...");
    let paired = accept_pairing(announcer.endpoint, code, Side::TypedCode, requests).await;
    announcer.withdraw().await;
    match paired? {
        Accepted::Paired(conn, stream) => session(announcer.endpoint, state, &conn, stream)
            .await
            .map(|()| Outcome::Done),
        Accepted::Spent => Ok(Outcome::Rejected),
        Accepted::Change(change) => Ok(Outcome::Change(change)),
    }
}

/// How waiting for a controller ended.
enum Accepted {
    Paired(Connection, Stream),
    /// [`MAX_FAILURES`] guesses were spent.
    Spent,
    /// The operator asked for another code, or other relays (with a code of
    /// our own only).
    Change(control::Change),
}

/// Accepts connections until one pairs with `code`, or the operator asks
/// for a change.
async fn accept_pairing(
    endpoint: &Endpoint,
    code: &Code,
    side: Side,
    requests: &mut Requests<'_>,
) -> Result<Accepted> {
    let mut failures = 0;
    loop {
        requests.idle.store(true, Ordering::Relaxed);
        let incoming = tokio::select! {
            incoming = endpoint.accept() => incoming.context("endpoint closed")?,
            Some(change) = requests.changes.recv() => {
                // Its asker gave up waiting.
                if change.answer.is_closed() {
                    continue;
                }
                if side == Side::TypedCode {
                    let answered = match &change.wanted {
                        control::Wanted::Relays { .. } => Some(Err(
                            "this machine waits for a controller whose code was given here, \
                             which uses no relays"
                                .to_string(),
                        )),
                        control::Wanted::Code(Some(wanted)) if wanted == code => Some(Ok(
                            "already waiting for the controller with that code".to_string(),
                        )),
                        control::Wanted::Code(_) => None,
                    };
                    if let Some(answer) = answered {
                        let _ = change.answer.send(answer);
                        continue;
                    }
                }
                requests.idle.store(false, Ordering::Relaxed);
                return Ok(Accepted::Change(change));
            }
        };
        requests.idle.store(false, Ordering::Relaxed);
        let conn = match tokio::time::timeout(net::CONNECT_TIMEOUT, incoming).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                warn!("incoming connection failed: {e}");
                continue;
            }
            Err(_) => continue,
        };
        let peer = conn.remote_id();
        if conn.alpn() != ALPN_CTRL {
            // A controller coming back to a session that is over.
            conn.close(VarInt::from_u32(1), b"no session");
            continue;
        }
        match net::accept_and_pair(endpoint, &conn, side, code).await {
            Ok(stream) => return Ok(Accepted::Paired(conn, stream)),
            Err(failure) => {
                conn.close(VarInt::from_u32(1), b"pairing failed");
                if failure.guess_spent {
                    failures += 1;
                    warn!(%peer, failures, "pairing failed: {}", failure.error);
                    if failures >= MAX_FAILURES {
                        return Ok(Accepted::Spent);
                    }
                } else {
                    warn!(%peer, "pairing attempt failed: {}", failure.error);
                }
            }
        }
    }
}

/// After pairing: introduce ourselves, then serve this controller until it
/// disconnects. If the connection breaks after approval, wait for the same
/// controller to come back and go on. Other controllers are refused
/// meanwhile.
async fn session(
    endpoint: &Endpoint,
    state: &StateDir,
    conn: &Connection,
    stream: Stream,
) -> Result<()> {
    let controller = conn.remote_id();
    info!(%controller, "paired with controller");
    state.write("paired-controller", &controller.to_string())?;
    screen::say(format!("Paired with controller {controller}."));

    let mut session = Session::default();
    let (comebacks, mut returns) = tokio::sync::mpsc::channel(4);
    let acceptor = tokio::spawn(accept_resumes(endpoint.clone(), controller, comebacks));
    let (mut conn, mut stream) = (conn.clone(), stream);
    let mut introduce = true;
    let result = loop {
        // Separate tasks: reading a frame inside `select!` would lose data
        // whenever another branch won.
        let streams = tokio::spawn(accept_streams(conn.clone(), session.approved.clone()));
        let (end, early) = serve(
            state,
            &conn,
            &mut stream,
            &mut session,
            introduce,
            &mut returns,
        )
        .await;
        streams.abort();
        introduce = false;
        let error = match end {
            Ok(None) => break Ok(()),
            Ok(Some(error)) => error,
            Err(e) => break Err(e),
        };
        // Before approval there is nothing to come back to. After `Finish`
        // the install is done: the controller may only have missed that.
        if !session.approved.load(Ordering::Relaxed) {
            warn!(%controller, "lost the controller ({error:#})");
            break Ok(());
        }
        let wait = if session.shutdown.is_some() {
            SHUTDOWN_GRACE
        } else {
            RESUME_WAIT
        };
        if early.is_some() {
            info!(%controller, "the controller lost the connection ({error:#}) and came back");
        } else {
            warn!(%controller, "lost the controller ({error:#}); waiting for it to come back");
            screen::say(format!(
                "Lost the controller. Waiting up to {} for it to come back...",
                humantime(wait)
            ));
        }
        let Some((back, resumed)) = wait_for_resume(early, &mut returns, &session, wait).await
        else {
            warn!(%controller, "the controller didn't come back; ending the session");
            screen::say("The controller didn't come back.");
            break Ok(());
        };
        info!(%controller, answered = session.answered, "the controller is back");
        screen::say("The controller is back.");
        (conn, stream) = (back, resumed);
    };
    acceptor.abort();
    if result.is_ok() {
        // After `Finished` the controller leaves at once; don't wait for one
        // that vanished without saying so.
        let wait = if session.shutdown.is_some() {
            SHUTDOWN_GRACE
        } else {
            Duration::MAX
        };
        if let Ok(reason) = tokio::time::timeout(wait, conn.closed()).await {
            info!(%controller, "controller disconnected: {reason}");
        } else {
            warn!(%controller, "controller did not disconnect");
        }
    }
    state.remove("paired-controller");
    state.remove("approved");
    result?;
    if let Some(how) = session.shutdown {
        shut_down(how).await?;
    }
    Ok(())
}

/// What a session keeps across connections to its controller.
#[derive(Default)]
struct Session {
    approved: Arc<AtomicBool>,
    /// Checked (and linked) by `PrepareDisks`, for `RunDisko`.
    prepared: Option<Vec<DiskAssignment>>,
    /// Set by a successful `Finish`.
    shutdown: Option<Shutdown>,
    /// Requests answered so far, and the last reply: a controller that
    /// comes back gets it again if it never saw it.
    answered: u64,
    last: Option<TargetMsg>,
}

/// How long a target waits for its controller to come back after their
/// connection broke. The controller reconnects as soon as it notices, even
/// while it is busy with something else, so this only has to cover the
/// network being away: a cable replugged, a laptop woken up. A controller
/// that crashed or was killed never comes back, and keeps the target from
/// pairing with another one until then.
const RESUME_WAIT: Duration = Duration::from_mins(10);

/// Serves the controller on `conn` until it hangs up (`None`) or the
/// connection is lost (the error). If the controller comes back meanwhile,
/// it lost the connection itself: it is closed here, and the returning
/// connection is the second value. A request in progress finishes either
/// way.
async fn serve(
    state: &StateDir,
    conn: &Connection,
    stream: &mut Stream,
    session: &mut Session,
    introduce: bool,
    returns: &mut tokio::sync::mpsc::Receiver<Connection>,
) -> (Result<Option<anyhow::Error>>, Option<Connection>) {
    let serving = serve_controller(state, conn, stream, session, introduce);
    tokio::pin!(serving);
    let (mut early, mut listening) = (None, true);
    let end = loop {
        tokio::select! {
            end = &mut serving => break end,
            back = returns.recv(), if listening => {
                listening = false;
                if let Some(back) = back {
                    conn.close(VarInt::from_u32(2), b"replaced");
                    early = Some(back);
                }
            }
        }
    };
    let end = end.map(|end| end.and_then(|error| is_lost(conn).then_some(error)));
    (end, early)
}

/// Whether `conn` broke rather than being closed by the controller: the
/// controller closes it at the end of a session, or when it is interrupted.
fn is_lost(conn: &Connection) -> bool {
    conn.close_reason()
        .is_some_and(|reason| !matches!(reason, ConnectionError::ApplicationClosed(_)))
}

/// Hands the session's controller's connections to resume it over
/// `returns`, and turns everyone else away.
async fn accept_resumes(
    endpoint: Endpoint,
    controller: EndpointId,
    returns: tokio::sync::mpsc::Sender<Connection>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let returns = returns.clone();
        tokio::spawn(async move {
            let Ok(Ok(conn)) = tokio::time::timeout(net::CONNECT_TIMEOUT, incoming).await else {
                return;
            };
            if conn.remote_id() == controller && conn.alpn() == ALPN_RESUME {
                let _ = returns.send(conn).await;
            } else {
                conn.close(VarInt::from_u32(1), b"busy");
            }
        });
    }
}

/// Waits up to `wait` for the controller to come back (unless it already
/// did, `early`), and resumes the session with it.
async fn wait_for_resume(
    early: Option<Connection>,
    returns: &mut tokio::sync::mpsc::Receiver<Connection>,
    session: &Session,
    wait: Duration,
) -> Option<(Connection, Stream)> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut early = early;
    loop {
        let conn = match early.take() {
            Some(conn) => conn,
            None => tokio::time::timeout_at(deadline, returns.recv())
                .await
                .ok()??,
        };
        match resume(&conn, session).await {
            Ok(stream) => return Some((conn, stream)),
            Err(e) => {
                warn!("the controller came back, but: {e:#}");
                conn.close(VarInt::from_u32(1), b"resume failed");
            }
        }
    }
}

/// The resume handshake (see `nix_pixie_proto::ctrl`): tells the controller
/// how many requests have their reply, and replays the last one if the
/// controller didn't get it.
async fn resume(conn: &Connection, session: &Session) -> Result<Stream> {
    let (send, recv) = tokio::time::timeout(net::CONNECT_TIMEOUT, conn.accept_bi())
        .await
        .context("timed out waiting for the control stream")??;
    let mut stream = tokio::io::join(recv, send);
    let ControllerMsg::Resume { answered } = tokio::time::timeout(
        net::CONNECT_TIMEOUT,
        read_frame(&mut stream, MAX_CONTROLLER_FRAME),
    )
    .await
    .context("timed out waiting for the controller to resume")??
    else {
        bail!("the controller didn't start with Resume");
    };
    write_frame(
        &mut stream,
        &TargetMsg::Resumed {
            answered: session.answered,
        },
    )
    .await?;
    if answered.checked_add(1) == Some(session.answered)
        && let Some(last) = &session.last
    {
        write_frame(&mut stream, last).await?;
    } else if answered != session.answered {
        bail!(
            "the controller has {answered} replies, but {} were sent",
            session.answered
        );
    }
    Ok(stream)
}

/// "10 minutes", "30 seconds".
fn humantime(wait: Duration) -> String {
    let secs = wait.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} minutes", secs / 60)
    } else {
        format!("{secs} seconds")
    }
}

/// Reboots into the installed system or powers off.
async fn shut_down(how: Shutdown) -> Result<()> {
    let (verb, message) = match how {
        Shutdown::Reboot => ("reboot", "Rebooting into the installed system."),
        Shutdown::PowerOff => ("poweroff", "Powering off."),
    };
    info!(how = verb, "installed; shutting down");
    screen::say(format!("Installed. {message}"));
    let status = tokio::process::Command::new("systemctl")
        .arg(verb)
        .status()
        .await
        .context("running systemctl")?;
    if !status.success() {
        bail!("systemctl {verb} failed ({status})");
    }
    Ok(())
}

/// Serves the extra streams the controller opens (store connections). Each
/// runs in a task owned by this one, so aborting it ends them all.
async fn accept_streams(conn: Connection, approved: Arc<AtomicBool>) {
    let mut tasks = JoinSet::new();
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        let approved = approved.clone();
        tasks.spawn(async move {
            let request = read_frame::<_, StreamRequest>(&mut recv, MAX_STREAM_REQUEST).await;
            let result = match request {
                Ok(_) if !approved.load(Ordering::Relaxed) => {
                    Err(anyhow::anyhow!("stream before approval"))
                }
                Ok(request) => serve_store(request, recv, send).await,
                Err(e) => Err(e.into()),
            };
            if let Err(e) = result {
                warn!("stream: {e:#}");
            }
        });
        // Reap finished tasks as we go.
        while tasks.try_join_next().is_some() {}
    }
}

/// Serves a store stream as `request` asks.
async fn serve_store(
    request: StreamRequest,
    recv: iroh::endpoint::RecvStream,
    send: iroh::endpoint::SendStream,
) -> Result<()> {
    let (request, zstd) = match request {
        StreamRequest::Zstd(inner) => (*inner, true),
        request => (request, false),
    };
    match request {
        StreamRequest::Store { root } => match store::check_root(&root) {
            Ok(_) => store::serve(Some(&root), recv, send, zstd).await,
            Err(reason) => Err(anyhow::anyhow!(reason)),
        },
        StreamRequest::LiveStore => store::serve(None, recv, send, zstd).await,
        StreamRequest::Zstd(_) => Err(anyhow::anyhow!("compressed twice")),
    }
}

/// Answers the paired controller's requests until it disconnects, rejects
/// us, or reading or writing fails (the error: whether that means the
/// connection broke is up to the connection). Disconnecting is the normal
/// end of a session; after `Finish`, it is the signal to shut down.
/// `introduce`: start with the hello and the inventory (not after resuming).
async fn serve_controller(
    state: &StateDir,
    conn: &Connection,
    stream: &mut Stream,
    session: &mut Session,
    introduce: bool,
) -> Result<Option<anyhow::Error>> {
    if introduce {
        let label = net::hostname();
        write_frame(stream, &TargetMsg::Hello { label }).await?;
        let inventory = inventory::collect(&disko::formatted(state)).await;
        write_frame(stream, &TargetMsg::Inventory(Box::new(inventory))).await?;
    }
    loop {
        let message = match read_frame::<_, ControllerMsg>(stream, MAX_CONTROLLER_FRAME).await {
            Ok(message) => message,
            Err(FrameError::Io(e)) => return Ok(Some(e.into())),
            Err(e) => {
                warn!("the controller broke the protocol: {e}");
                // Said at once, rather than leaving it waiting for a reply:
                // most likely, it is another version of nix-pixie.
                let reason = net::sanitise(
                    &format!(
                        "a message this agent can't read ({e}); it speaks nix-pixie protocol {}",
                        nix_pixie_proto::PROTOCOL_VERSION
                    ),
                    200,
                );
                conn.close(VarInt::from_u32(PROTOCOL_BROKEN), reason.as_bytes());
                return Ok(None);
            }
        };
        let reply = match message {
            ControllerMsg::Approve { machine } => {
                approve(state, &session.approved, machine)?;
                continue;
            }
            ControllerMsg::Reject { reason } => {
                let reason = net::sanitise(&reason, 200);
                warn!(%reason, "rejected by the controller");
                screen::say(format!("Rejected by the controller: {reason}"));
                return Ok(None);
            }
            // Only first on a new connection.
            ControllerMsg::Resume { .. } => {
                warn!("the controller sent Resume in the middle of a session");
                return Ok(None);
            }
            // Everything else needs approval first.
            message if !session.approved.load(Ordering::Relaxed) => match refusal(&message) {
                Some(reply) => reply,
                None => continue,
            },
            message => answer(state, stream, session, message).await?,
        };
        // Kept before it is sent: if sending fails, the controller comes
        // back for it.
        session.answered += 1;
        session.last = Some(reply.clone());
        if let Err(e) = write_frame(stream, &reply).await {
            return Ok(Some(match e {
                FrameError::Io(e) => e.into(),
                e => e.into(),
            }));
        }
    }
}

/// The reply to an approved request.
async fn answer(
    state: &StateDir,
    stream: &mut Stream,
    session: &mut Session,
    message: ControllerMsg,
) -> Result<TargetMsg> {
    Ok(match message {
        ControllerMsg::RequestFacterReport => facter_report(state).await,
        ControllerMsg::RequestAddresses => TargetMsg::Addresses {
            addrs: net::own_addresses(),
        },
        ControllerMsg::SetClock { unix_ms } => crate::clock::set(unix_ms).await,
        ControllerMsg::AddBootEntry { esp } => add_boot_entry(&esp).await,
        ControllerMsg::PrepareStore { root } => prepare_store(&root),
        ControllerMsg::PrepareDisks { disks, labels } => {
            session.prepared = None;
            prepare_disks(state, disks, &labels, &mut session.prepared).await
        }
        ControllerMsg::RunDisko { script, key_files } => {
            run_disko(state, stream, &script, &key_files, session.prepared.take()).await?
        }
        ControllerMsg::GenerateHostKey => generate_host_key(state).await,
        ControllerMsg::ProvideHostKey { private } => {
            match nixos::store_host_key(state, &private.0).await {
                Ok(public) => TargetMsg::HostKey { public },
                Err(e) => {
                    warn!("pushed host key: {e:#}");
                    TargetMsg::HostKeyFailed {
                        error: format!("{e:#}"),
                    }
                }
            }
        }
        ControllerMsg::Install {
            root,
            system,
            host_key_path,
        } => install(state, stream, &root, &system, host_key_path.as_deref()).await?,
        ControllerMsg::PlaceFiles { root, files } => match nixos::place_files(&root, &files) {
            Ok(()) => TargetMsg::FilesPlaced,
            Err(reason) => {
                warn!(%reason, "refusing to place files");
                TargetMsg::FilesRefused { reason }
            }
        },
        ControllerMsg::Exec {
            program,
            args,
            root,
            input,
        } => exec(stream, &program, &args, root.as_deref(), input).await?,
        ControllerMsg::Finish { root, zpools, then } => {
            let reply = finish(&root, &zpools).await;
            if matches!(reply, TargetMsg::Finished { .. }) {
                session.shutdown = Some(then);
            }
            reply
        }
        // Handled before.
        ControllerMsg::Approve { .. }
        | ControllerMsg::Reject { .. }
        | ControllerMsg::Resume { .. } => TargetMsg::ExecFailed {
            error: "unexpected".into(),
        },
    })
}

/// A boot entry for the installed system's systemd-boot, first in the
/// firmware's boot order (see [`crate::bootentry`]).
async fn add_boot_entry(esp: &str) -> TargetMsg {
    let esp = Path::new(esp);
    if !esp.is_absolute() {
        return TargetMsg::BootEntryFailed {
            error: format!("{} is not an absolute path", esp.display()),
        };
    }
    match crate::bootentry::add(esp).await {
        Ok((number, created)) => {
            screen::say(format!(
                "The firmware boots the installed system next (Boot{number}, \"{}\").",
                crate::bootentry::LABEL
            ));
            TargetMsg::BootEntry { number, created }
        }
        Err(e) => {
            warn!("boot entry: {e:#}");
            TargetMsg::BootEntryFailed {
                error: format!("{e:#}"),
            }
        }
    }
}

/// Checks the disks the controller chose, and keeps them for `RunDisko`.
async fn prepare_disks(
    state: &StateDir,
    disks: Vec<DiskAssignment>,
    labels: &[String],
    prepared: &mut Option<Vec<DiskAssignment>>,
) -> TargetMsg {
    match disko::prepare(&disks, labels, &disko::formatted(state)).await {
        Ok(()) => {
            let names: Vec<String> = disks
                .iter()
                .map(|d| format!("{} ({})", d.device, d.role))
                .collect();
            info!(disks = ?names, "disks ready to format");
            screen::say(format!("About to format: {}", names.join(", ")));
            *prepared = Some(disks);
            TargetMsg::DisksReady
        }
        Err(reason) => {
            warn!(%reason, "refusing to format");
            TargetMsg::DisksRefused { reason }
        }
    }
}

fn approve(state: &StateDir, approved: &AtomicBool, machine: Option<String>) -> Result<()> {
    approved.store(true, Ordering::Relaxed);
    let machine = machine.map(|m| net::sanitise(&m, 63));
    state.write("approved", machine.as_deref().unwrap_or(""))?;
    info!(?machine, "approved by the controller");
    match machine {
        Some(machine) => screen::say(format!(
            "Approved as {machine}. Waiting for the controller."
        )),
        None => screen::say("Approved. Waiting for the controller."),
    }
    Ok(())
}

/// The reply to a request that needs approval, sent before it. `None` for
/// messages that need none.
fn refusal(message: &ControllerMsg) -> Option<TargetMsg> {
    let reason = "not approved yet".to_string();
    match message {
        ControllerMsg::Approve { .. }
        | ControllerMsg::Reject { .. }
        | ControllerMsg::Resume { .. } => None,
        ControllerMsg::RequestFacterReport => Some(TargetMsg::FacterFailed { error: reason }),
        ControllerMsg::PrepareStore { .. } => Some(TargetMsg::StoreRefused { reason }),
        ControllerMsg::PrepareDisks { .. } => Some(TargetMsg::DisksRefused { reason }),
        ControllerMsg::RunDisko { .. } => Some(TargetMsg::DiskoFailed { error: reason }),
        ControllerMsg::GenerateHostKey | ControllerMsg::ProvideHostKey { .. } => {
            Some(TargetMsg::HostKeyFailed { error: reason })
        }
        ControllerMsg::Install { .. } => Some(TargetMsg::InstallFailed { error: reason }),
        ControllerMsg::Finish { .. } => Some(TargetMsg::FinishFailed { error: reason }),
        ControllerMsg::PlaceFiles { .. } => Some(TargetMsg::FilesRefused { reason }),
        ControllerMsg::Exec { .. } => Some(TargetMsg::ExecFailed { error: reason }),
        ControllerMsg::SetClock { .. } => Some(TargetMsg::ClockFailed { error: reason }),
        ControllerMsg::AddBootEntry { .. } => Some(TargetMsg::BootEntryFailed { error: reason }),
        // None, rather than a message just for refusing this.
        ControllerMsg::RequestAddresses => Some(TargetMsg::Addresses { addrs: Vec::new() }),
    }
}

/// Runs a hook's program, which the controller pushed here (see
/// [`crate::exec`]).
async fn exec(
    stream: &mut Stream,
    program: &str,
    args: &[String],
    root: Option<&str>,
    input: Option<Secret>,
) -> Result<TargetMsg> {
    let command = match exec::command(program, args, root) {
        Ok(command) => command,
        Err(error) => {
            warn!(
                error = net::sanitise(&error, 600),
                "not running a hook's program"
            );
            return Ok(TargetMsg::ExecFailed { error });
        }
    };
    info!(
        program = net::sanitise(program, 512),
        args = args.len(),
        ?root,
        "running a hook's program"
    );
    child::exec(command, stream, input.map(|secret| secret.0)).await
}

async fn facter_report(state: &StateDir) -> TargetMsg {
    match inventory::facter_report(state.path()).await {
        Ok(json) => TargetMsg::FacterReport { json },
        Err(e) => {
            warn!("facter report: {e:#}");
            TargetMsg::FacterFailed {
                error: format!("{e:#}"),
            }
        }
    }
}

fn prepare_store(root: &str) -> TargetMsg {
    match store::check_root(root) {
        Ok(fstype) => {
            info!(%root, %fstype, "ready to receive store paths");
            screen::say(format!("Receiving the system into {root} ({fstype})."));
            TargetMsg::StoreReady { fstype }
        }
        Err(reason) => {
            warn!(%root, %reason, "refusing to receive store paths");
            TargetMsg::StoreRefused { reason }
        }
    }
}

async fn generate_host_key(state: &StateDir) -> TargetMsg {
    let comment = state
        .read("approved")
        .filter(|m| !m.is_empty())
        .map_or_else(String::new, |machine| format!("root@{machine}"));
    match nixos::generate_host_key(state, &comment).await {
        Ok(public) => TargetMsg::HostKey { public },
        Err(e) => {
            warn!("host key: {e:#}");
            TargetMsg::HostKeyFailed {
                error: format!("{e:#}"),
            }
        }
    }
}

async fn install(
    state: &StateDir,
    stream: &mut Stream,
    root: &str,
    system: &str,
    host_key_path: Option<&str>,
) -> Result<TargetMsg> {
    screen::say("Installing the boot loader...");
    Ok(
        match nixos::install(state, stream, root, system, host_key_path).await? {
            Ok(()) => TargetMsg::InstallDone,
            Err(error) => {
                warn!(%error, "nixos-install");
                screen::say(format!("Installing failed: {error}"));
                TargetMsg::InstallFailed { error }
            }
        },
    )
}

async fn finish(root: &str, zpools: &[String]) -> TargetMsg {
    match nixos::finish(root, zpools).await {
        Ok(()) => {
            // So the controller can tell the installer from the installed
            // system while the target reboots.
            let live_host_key = std::fs::read_to_string("/etc/ssh/ssh_host_ed25519_key.pub")
                .ok()
                .map(|key| key.trim().to_string());
            TargetMsg::Finished { live_host_key }
        }
        Err(error) => {
            warn!(%error, "finishing");
            TargetMsg::FinishFailed { error }
        }
    }
}

async fn run_disko(
    state: &StateDir,
    stream: &mut Stream,
    script: &str,
    key_files: &[KeyFile],
    prepared: Option<Vec<DiskAssignment>>,
) -> Result<TargetMsg> {
    let Some(disks) = prepared else {
        return Ok(TargetMsg::DiskoFailed {
            error: "the disks were not prepared".to_string(),
        });
    };
    screen::say("Formatting...");
    Ok(
        match disko::run(state, stream, script, &disks, key_files).await? {
            Ok(()) => {
                screen::say("Formatted and mounted.");
                TargetMsg::DiskoDone
            }
            Err(error) => {
                warn!(%error, "disko");
                screen::say(format!("Formatting failed: {error}"));
                TargetMsg::DiskoFailed { error }
            }
        },
    )
}
