//! The installer-side agent: announces itself, waits for a controller to dial
//! in and pair, and (for now) holds the session open until the controller
//! leaves.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, SecretKey};
use mdns_sd::ServiceEvent;
use nix_pixie_proto::announce::{Announcement, Pairing};
use nix_pixie_proto::code::Code;
use nix_pixie_proto::ctrl::{
    ControllerMsg, DiskAssignment, MAX_CONTROLLER_FRAME, MAX_STREAM_REQUEST, Shutdown,
    StreamRequest, TargetMsg,
};
use nix_pixie_proto::frame::{read_frame, write_frame};
use nix_pixie_proto::pair::Side;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::disko;
use crate::inventory;
use crate::mdns::{self, Mdns};
use crate::net::{self, Stream};
use crate::nixos;
use crate::probe::Responder;
use crate::state::StateDir;
use crate::store;

/// Spent guesses before a code is given up: the owner shows a new one, the
/// typist asks for it to be entered again. Each guess has ~1 in 2^31 odds.
pub const MAX_FAILURES: u32 = 3;
/// How long to wait for the controller to leave after `Finished` before
/// shutting down anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// How long to listen for other targets' nameplates before picking ours.
const NAMEPLATE_SURVEY: Duration = Duration::from_secs(2);

/// How `nix-pixie agent` was invoked.
pub struct AgentArgs {
    pub state_dir: PathBuf,
    pub port: u16,
    pub controller_code: Option<String>,
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
    // A fresh identity per boot. Nothing persists on the installer.
    let endpoint = net::bind(SecretKey::generate(), args.port).await?;
    let mdns = Mdns::new()?;
    let responder = Responder::bind().await?;
    let (announce_tx, announce_rx) = watch::channel(None);
    let responder = tokio::spawn(responder.run(announce_rx));
    state.write("endpoint-id", &endpoint.id().to_string())?;
    info!(id = %endpoint.id(), port = net::bound_port(&endpoint)?, "agent started");

    let mut announcer = Announcer {
        endpoint: &endpoint,
        mdns: &mdns,
        probe: &announce_tx,
        advertised: None,
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
    responder.abort();
    mdns.shutdown();
    endpoint.close().await;
    result
}

/// Publishes our announcement via DNS-SD and probe replies.
struct Announcer<'a> {
    endpoint: &'a Endpoint,
    mdns: &'a Mdns,
    probe: &'a watch::Sender<Option<Announcement>>,
    advertised: Option<String>,
}

impl Announcer<'_> {
    fn announce(&mut self, pairing: Pairing) -> Result<()> {
        self.withdraw();
        let announcement = Announcement::new(
            *self.endpoint.id().as_bytes(),
            net::bound_port(self.endpoint)?,
            pairing,
            &net::hostname(),
        );
        self.advertised = Some(self.mdns.advertise(&announcement)?);
        self.probe.send_replace(Some(announcement));
        Ok(())
    }

    fn withdraw(&mut self) {
        if let Some(name) = self.advertised.take() {
            self.mdns.withdraw(&name);
        }
        self.probe.send_replace(None);
    }
}

/// Default mode: show a code, let the operator type it on the controller.
async fn show_code(announcer: &mut Announcer<'_>, state: &StateDir) -> Result<()> {
    let endpoint = announcer.endpoint;
    loop {
        let taken = nameplates_in_use(announcer.mdns, endpoint).await?;
        let nameplate =
            Code::free_nameplate(&taken).context("all nameplates on this network are in use")?;
        let code = Code::generate(Some(nameplate))?;
        announcer.announce(Pairing::ShowsCode { nameplate })?;
        state.write("code", &code.to_string())?;
        println!(
            "\n    nix-pixie pairing code:  {code}\n\n    On the controller: nix-pixie controller pair --code {code}\n"
        );

        let paired = accept_pairing(endpoint, &code, Side::OwnsCode).await;
        announcer.withdraw();
        state.remove("code");
        if let Some((conn, stream)) = paired? {
            if let Err(e) = session(endpoint, state, &conn, stream).await {
                warn!("session ended with an error: {e:#}");
            }
        } else {
            warn!(
                "{MAX_FAILURES} pairing attempts with a wrong code; someone may be guessing. Showing a new code."
            );
        }
    }
}

/// Nameplates other targets currently advertise via DNS-SD.
async fn nameplates_in_use(mdns: &Mdns, endpoint: &Endpoint) -> Result<Vec<u8>> {
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
    announcer.announce(Pairing::TypedControllerCode)?;
    println!("\n    Waiting for the controller to connect...\n");
    let paired = accept_pairing(announcer.endpoint, code, Side::TypedCode).await;
    announcer.withdraw();
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
/// disconnects. Other controllers are refused meanwhile.
async fn session(
    endpoint: &Endpoint,
    state: &StateDir,
    conn: &Connection,
    mut stream: Stream,
) -> Result<()> {
    let controller = conn.remote_id();
    info!(%controller, "paired with controller");
    state.write("paired-controller", &controller.to_string())?;
    println!("\n    Paired with controller {controller}.\n");

    // Separate tasks: reading a frame inside `select!` would lose data
    // whenever another branch won.
    let approved = Arc::new(AtomicBool::new(false));
    let refuser = tokio::spawn(refuse_incoming(endpoint.clone()));
    let streams = tokio::spawn(accept_streams(conn.clone(), approved.clone()));
    let result = serve_controller(state, &mut stream, &approved).await;
    streams.abort();
    refuser.abort();
    if let Ok(shutdown) = &result {
        // After `Finished` the controller leaves at once; don't wait for one
        // that vanished without saying so.
        let wait = if shutdown.is_some() {
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
    if let Some(how) = result? {
        shut_down(how).await?;
    }
    Ok(())
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

async fn refuse_incoming(endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        incoming.refuse();
    }
}

/// Answers the paired controller's requests until it disconnects or rejects
/// us. Disconnecting is the normal end of a session; after `Finish`, it is
/// the signal to shut down as returned.
async fn serve_controller(
    state: &StateDir,
    stream: &mut Stream,
    approved: &AtomicBool,
) -> Result<Option<Shutdown>> {
    let label = net::hostname();
    write_frame(stream, &TargetMsg::Hello { label }).await?;
    let inventory = inventory::collect(&disko::formatted(state)).await;
    write_frame(stream, &TargetMsg::Inventory(Box::new(inventory))).await?;

    // Checked (and linked) by `PrepareDisks`, for `RunDisko`.
    let mut prepared: Option<Vec<DiskAssignment>> = None;
    // Set by a successful `Finish`.
    let mut shutdown: Option<Shutdown> = None;
    loop {
        let Ok(message) = read_frame::<_, ControllerMsg>(stream, MAX_CONTROLLER_FRAME).await else {
            return Ok(shutdown);
        };
        let reply = match message {
            ControllerMsg::Approve { machine } => {
                approve(state, approved, machine)?;
                continue;
            }
            ControllerMsg::Reject { reason } => {
                let reason = net::sanitise(&reason, 200);
                warn!(%reason, "rejected by the controller");
                println!("\n    Rejected by the controller: {reason}\n");
                return Ok(None);
            }
            // Everything else needs approval first.
            message if !approved.load(Ordering::Relaxed) => match refusal(&message) {
                Some(reply) => reply,
                None => continue,
            },
            ControllerMsg::RequestFacterReport => facter_report(state).await,
            ControllerMsg::PrepareStore { root } => prepare_store(&root),
            ControllerMsg::PrepareDisks { disks, labels } => {
                prepared = None;
                match disko::prepare(&disks, &labels, &disko::formatted(state)).await {
                    Ok(()) => {
                        let names: Vec<String> = disks
                            .iter()
                            .map(|d| format!("{} ({})", d.device, d.role))
                            .collect();
                        info!(disks = ?names, "disks ready to format");
                        println!("\n    About to format: {}\n", names.join(", "));
                        prepared = Some(disks);
                        TargetMsg::DisksReady
                    }
                    Err(reason) => {
                        warn!(%reason, "refusing to format");
                        TargetMsg::DisksRefused { reason }
                    }
                }
            }
            ControllerMsg::RunDisko { script } => {
                run_disko(state, stream, &script, prepared.take()).await?
            }
            ControllerMsg::GenerateHostKey => {
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
            ControllerMsg::Install {
                root,
                system,
                host_key_path,
            } => {
                println!("\n    Installing the boot loader...\n");
                match nixos::install(state, stream, &root, &system, host_key_path.as_deref())
                    .await?
                {
                    Ok(()) => TargetMsg::InstallDone,
                    Err(error) => {
                        warn!(%error, "nixos-install");
                        println!("\n    Installing failed: {error}\n");
                        TargetMsg::InstallFailed { error }
                    }
                }
            }
            ControllerMsg::Finish { root, zpools, then } => {
                match nixos::finish(&root, &zpools).await {
                    Ok(()) => {
                        shutdown = Some(then);
                        TargetMsg::Finished
                    }
                    Err(error) => {
                        warn!(%error, "finishing");
                        TargetMsg::FinishFailed { error }
                    }
                }
            }
        };
        write_frame(stream, &reply).await?;
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
        ControllerMsg::Approve { .. } | ControllerMsg::Reject { .. } => None,
        ControllerMsg::RequestFacterReport => Some(TargetMsg::FacterFailed { error: reason }),
        ControllerMsg::PrepareStore { .. } => Some(TargetMsg::StoreRefused { reason }),
        ControllerMsg::PrepareDisks { .. } => Some(TargetMsg::DisksRefused { reason }),
        ControllerMsg::RunDisko { .. } => Some(TargetMsg::DiskoFailed { error: reason }),
        ControllerMsg::GenerateHostKey => Some(TargetMsg::HostKeyFailed { error: reason }),
        ControllerMsg::Install { .. } => Some(TargetMsg::InstallFailed { error: reason }),
        ControllerMsg::Finish { .. } => Some(TargetMsg::FinishFailed { error: reason }),
    }
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

async fn run_disko(
    state: &StateDir,
    stream: &mut Stream,
    script: &str,
    prepared: Option<Vec<DiskAssignment>>,
) -> Result<TargetMsg> {
    let Some(disks) = prepared else {
        return Ok(TargetMsg::DiskoFailed {
            error: "the disks were not prepared".to_string(),
        });
    };
    println!("\n    Formatting...\n");
    Ok(match disko::run(state, stream, script, &disks).await? {
        Ok(()) => {
            println!("\n    Formatted and mounted.\n");
            TargetMsg::DiskoDone
        }
        Err(error) => {
            warn!(%error, "disko");
            println!("\n    Formatting failed: {error}\n");
            TargetMsg::DiskoFailed { error }
        }
    })
}
