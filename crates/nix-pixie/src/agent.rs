//! The installer-side agent: announces itself, waits for a controller to dial
//! in and pair, and (for now) holds the session open until the controller
//! leaves.

use std::fmt::Write as _;
use std::path::PathBuf;
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
    ControllerMsg, DiskAssignment, KeyFile, MAX_CONTROLLER_FRAME, MAX_STREAM_REQUEST, Shutdown,
    StreamRequest, TargetMsg,
};
use nix_pixie_proto::frame::{FrameError, read_frame, write_frame};
use nix_pixie_proto::pair::Side;
use nix_pixie_proto::{ALPN_CTRL, ALPN_RESUME};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::child;
use crate::disko;
use crate::exec;
use crate::inventory;
use crate::mdns::{self, Mdns};
use crate::net::{self, Relays, Stream};
use crate::nixos;
use crate::probe::Responder;
use crate::relay_status::{self, Reach};
use crate::rendezvous::Rendezvous;
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
    // formatted and the host key it made carry over, its session doesn't.
    for stale in ["code", "paired-controller", "approved", "relay"] {
        state.remove(stale);
    }
    // A controller's code has no nameplate to meet at: without one, relays
    // are of no use.
    let relays = if controller_code.is_some() {
        Relays::Disabled
    } else {
        args.relays
    };
    // A fresh identity per boot. Nothing persists on the installer.
    let endpoint = net::bind(SecretKey::generate(), args.port, &relays).await?;
    let mdns = Mdns::new()?;
    let responder = Responder::bind().await?;
    let (announce_tx, announce_rx) = watch::channel(None);
    let responder = tokio::spawn(responder.run(announce_rx));
    state.write("endpoint-id", &endpoint.id().to_string())?;
    info!(id = %endpoint.id(), port = net::bound_port(&endpoint)?, "agent started");

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
        mdns: &mdns,
        probe: &announce_tx,
        relays: &relays,
        reach,
        state: &state,
        advertised: None,
        rendezvous: None,
    };
    let result = tokio::select! {
        r = async {
            match &controller_code {
                None => show_code(&mut announcer, &state).await,
                Some(code) => typed_code(&mut announcer, &state, code).await,
            }
        } => r,
        () = crate::shutdown_signal() => Ok(()),
    };
    announcer.withdraw().await;
    if let Some(task) = watching {
        task.abort();
    }
    responder.abort();
    mdns.shutdown();
    endpoint.close().await;
    result
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

    /// How the relay stands, as a line of the code's banner (nothing
    /// without relays).
    fn relay_line(&self) -> String {
        self.reach.as_ref().map_or_else(String::new, |reach| {
            format!("    {}\n", reach.borrow().line(&self.relays.to_string()))
        })
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
async fn show_code(announcer: &mut Announcer<'_>, state: &StateDir) -> Result<()> {
    let endpoint = announcer.endpoint;
    let mut rotations = 0;
    loop {
        let taken = nameplates_in_use(announcer.mdns, endpoint).await?;
        let nameplate = Code::free_nameplate(announcer.nameplate_space(), &taken)
            .context("all nameplates on this network are in use")?;
        let code = Code::generate(Some(nameplate))?;
        announcer.announce(Pairing::ShowsCode { nameplate }).await?;
        state.write("code", &code.to_string())?;
        println!(
            "\n    nix-pixie pairing code:  {code}\n\n    On the controller: nix-pixie controller pair --code {code}{}\n{}",
            announcer.controller_flags(),
            announcer.relay_line()
        );

        let paired = accept_pairing(endpoint, &code, Side::OwnsCode).await;
        announcer.withdraw().await;
        state.remove("code");
        if let Some((conn, stream)) = paired? {
            if let Err(e) = session(endpoint, state, &conn, stream).await {
                warn!("session ended with an error: {e:#}");
            }
        } else {
            rotations += 1;
            let delay = rotation_delay(rotations);
            warn!(
                "{MAX_FAILURES} pairing attempts with a wrong code; someone may be guessing. Showing a new code in {}s.",
                delay.as_secs()
            );
            tokio::time::sleep(delay).await;
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

/// Alternative mode: the operator typed the controller's code here. The
/// controller finds us and dials in.
async fn typed_code(announcer: &mut Announcer<'_>, state: &StateDir, code: &Code) -> Result<()> {
    announcer.announce(Pairing::TypedControllerCode).await?;
    println!("\n    Waiting for the controller to connect...\n");
    let paired = accept_pairing(announcer.endpoint, code, Side::TypedCode).await;
    announcer.withdraw().await;
    match paired? {
        Some((conn, stream)) => session(announcer.endpoint, state, &conn, stream).await,
        None => bail!(
            "the code was rejected {MAX_FAILURES} times; check the code shown by the controller and enter it again"
        ),
    }
}

/// Accepts connections until one pairs with `code`. `None` after
/// [`MAX_FAILURES`] spent guesses.
async fn accept_pairing(
    endpoint: &Endpoint,
    code: &Code,
    side: Side,
) -> Result<Option<(Connection, Stream)>> {
    let mut failures = 0;
    loop {
        let incoming = endpoint.accept().await.context("endpoint closed")?;
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
            Ok(stream) => return Ok(Some((conn, stream))),
            Err(failure) => {
                conn.close(VarInt::from_u32(1), b"pairing failed");
                if failure.guess_spent {
                    failures += 1;
                    warn!(%peer, failures, "pairing failed: {}", failure.error);
                    if failures >= MAX_FAILURES {
                        return Ok(None);
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
    println!("\n    Paired with controller {controller}.\n");

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
            println!(
                "\n    Lost the controller. Waiting up to {} for it to come back...\n",
                humantime(wait)
            );
        }
        let Some((back, resumed)) = wait_for_resume(early, &mut returns, &session, wait).await
        else {
            warn!(%controller, "the controller didn't come back; ending the session");
            println!("\n    The controller didn't come back.\n");
            break Ok(());
        };
        info!(%controller, answered = session.answered, "the controller is back");
        println!("\n    The controller is back.\n");
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
    let serving = serve_controller(state, stream, session, introduce);
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
    println!("\n    Installed. {message}\n");
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
                Ok(StreamRequest::Store { root }) => match store::check_root(&root) {
                    Ok(_) => store::serve(Some(&root), recv, send).await,
                    Err(reason) => Err(anyhow::anyhow!(reason)),
                },
                Ok(StreamRequest::LiveStore) => store::serve(None, recv, send).await,
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

/// Answers the paired controller's requests until it disconnects, rejects
/// us, or reading or writing fails (the error: whether that means the
/// connection broke is up to the connection). Disconnecting is the normal
/// end of a session; after `Finish`, it is the signal to shut down.
/// `introduce`: start with the hello and the inventory (not after resuming).
async fn serve_controller(
    state: &StateDir,
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
                println!("\n    Rejected by the controller: {reason}\n");
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
            ControllerMsg::RequestFacterReport => facter_report(state).await,
            ControllerMsg::RequestAddresses => TargetMsg::Addresses {
                addrs: net::own_addresses(),
            },
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
            } => exec(stream, &program, &args, root.as_deref()).await?,
            ControllerMsg::Finish { root, zpools, then } => {
                let reply = finish(&root, &zpools).await;
                if matches!(reply, TargetMsg::Finished { .. }) {
                    session.shutdown = Some(then);
                }
                reply
            }
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
            println!("\n    About to format: {}\n", names.join(", "));
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
        Some(machine) => println!("\n    Approved as {machine}. Waiting for the controller.\n"),
        None => println!("\n    Approved. Waiting for the controller.\n"),
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
    child::exec(command, stream).await
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
            println!("\n    Receiving the system into {root} ({fstype}).\n");
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
    println!("\n    Installing the boot loader...\n");
    Ok(
        match nixos::install(state, stream, root, system, host_key_path).await? {
            Ok(()) => TargetMsg::InstallDone,
            Err(error) => {
                warn!(%error, "nixos-install");
                println!("\n    Installing failed: {error}\n");
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
    println!("\n    Formatting...\n");
    Ok(
        match disko::run(state, stream, script, &disks, key_files).await? {
            Ok(()) => {
                println!("\n    Formatted and mounted.\n");
                TargetMsg::DiskoDone
            }
            Err(error) => {
                warn!(%error, "disko");
                println!("\n    Formatting failed: {error}\n");
                TargetMsg::DiskoFailed { error }
            }
        },
    )
}
