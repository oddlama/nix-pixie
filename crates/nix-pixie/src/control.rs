//! The agent's control socket, `control.sock` in its state directory,
//! which only root can enter. `nix-pixie relay` changes the agent's relays
//! through it, `nix-pixie code` gives it a controller's code to wait for,
//! and `nix-pixie console` follows its [`Status`] and does both.
//!
//! Frames as everywhere else (see `nix_pixie_proto::frame`), one request per
//! connection.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix_pixie_proto::code::Code;
use nix_pixie_proto::frame::{read_frame, write_frame};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};

use crate::net::Relays;
use crate::screen::{self, Status};

/// The socket's name in the agent's state directory.
pub const SOCKET: &str = "control.sock";
/// The state file with relays chosen through the socket: `off`, `n0`, or
/// URLs, one per line. It lasts the boot, like the state directory.
pub const CHOSEN: &str = "relays";
/// The state file with a controller's code given through the socket. It
/// lasts the boot, so a restarted agent still waits for that controller.
pub const GIVEN_CODE: &str = "controller-code";
/// Upper bound for a frame on the socket.
const MAX_FRAME: usize = 256 * 1024;
/// How long a relay change may wait for the agent to take it.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a change waits for the agent to wait for controllers again:
/// after the last change, it takes a moment to listen anew.
const SETTLE: Duration = Duration::from_secs(3);

/// What a client asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The screen, now and whenever it changes.
    Watch,
    /// Use other relays.
    Relays(Setting),
    /// Wait for the controller with this code (three words); `None`: as
    /// the agent was started.
    Code(Option<String>),
}

/// Relays as the operator asks for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Setting {
    /// What the agent was started with.
    Configured,
    Off,
    /// n0's public relays.
    N0,
    Urls(Vec<String>),
}

impl Setting {
    /// From words typed by the operator: `off`, `n0`, `default`, or URLs.
    pub fn parse(words: &[String]) -> Result<Self> {
        Ok(match words {
            [] => bail!("which relays? A URL, `off`, `n0` or `default`"),
            [word] if word == "off" => Self::Off,
            [word] if word == "n0" => Self::N0,
            [word] if word == "default" => Self::Configured,
            urls => {
                // Checked here too, for an error before anything is sent.
                Relays::from_args(urls, false)?;
                Self::Urls(urls.to_vec())
            }
        })
    }
}

/// What the agent answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Status(Status),
    Done(String),
    Refused(String),
}

/// What the operator asks the agent's run loop for, and where its answer
/// goes.
pub struct Change {
    pub wanted: Wanted,
    pub answer: oneshot::Sender<Result<String, String>>,
}

/// A change to how the agent waits for controllers.
pub enum Wanted {
    Relays {
        relays: Relays,
        /// For [`CHOSEN`]; `None` to go back to the configured relays.
        chosen: Option<String>,
    },
    /// Wait for the controller with this code instead of showing one; `None`:
    /// as the agent was started.
    Code(Option<Code>),
}

/// What the agent's run loop shares with the socket.
pub struct Agent {
    /// The relays it was started with.
    pub configured: Relays,
    /// Changes go here, while [`Self::idle`].
    pub changes: mpsc::Sender<Change>,
    /// Whether it waits for a controller, rather than pairing or serving
    /// one: only then may the relays or the code change.
    pub idle: Arc<AtomicBool>,
}

/// Serves the socket at `path` until the task is aborted.
pub async fn serve(path: PathBuf, agent: Arc<Agent>) -> Result<()> {
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accepting on the control socket")?;
        let agent = agent.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &agent).await {
                debug!("control socket: {e:#}");
            }
        });
    }
}

async fn handle(mut stream: UnixStream, agent: &Agent) -> Result<()> {
    match read_frame(&mut stream, MAX_FRAME).await? {
        Request::Watch => {
            let mut status = screen::watch();
            loop {
                let now = status.borrow_and_update().clone();
                write_frame(&mut stream, &Reply::Status(now)).await?;
                if status.changed().await.is_err() {
                    return Ok(());
                }
            }
        }
        Request::Relays(setting) => {
            let reply = match relays(agent, &setting) {
                Ok(wanted) => change(agent, wanted).await,
                Err(refused) => Err(refused),
            };
            answer(&mut stream, reply).await
        }
        Request::Code(code) => {
            let reply = match code.as_deref().map(controller_code).transpose() {
                Ok(code) => change(agent, Wanted::Code(code)).await,
                Err(refused) => Err(refused),
            };
            answer(&mut stream, reply).await
        }
    }
}

async fn answer(stream: &mut UnixStream, reply: Result<String, String>) -> Result<()> {
    let reply = match reply {
        Ok(done) => Reply::Done(done),
        Err(refused) => Reply::Refused(refused),
    };
    write_frame(stream, &reply).await?;
    Ok(())
}

/// The relays the operator asks for.
fn relays(agent: &Agent, setting: &Setting) -> Result<Wanted, String> {
    let (relays, chosen) = match setting {
        Setting::Configured => (agent.configured.clone(), None),
        Setting::Off => (Relays::Disabled, Some("off".to_string())),
        Setting::N0 => (Relays::N0, Some("n0".to_string())),
        Setting::Urls(urls) => (
            Relays::from_args(urls, false).map_err(|e| format!("{e:#}"))?,
            Some(urls.join("\n")),
        ),
    };
    Ok(Wanted::Relays { relays, chosen })
}

/// `text` as a controller's code: three words, no nameplate.
///
/// # Errors
///
/// If it isn't one, saying why.
pub fn controller_code(text: &str) -> Result<Code, String> {
    let code = Code::parse(text).map_err(|e| format!("{e:#}"))?;
    if code.nameplate().is_some() {
        return Err(
            "that code starts with a number, so it is a target's; give the code of the controller"
                .into(),
        );
    }
    Ok(code)
}

/// Hands a change to the run loop, once it can take it.
async fn change(agent: &Agent, wanted: Wanted) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + SETTLE;
    while !agent.idle.load(Ordering::Relaxed) {
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "a controller is pairing or paired right now; try again once it is done".into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match &wanted {
        Wanted::Relays { relays, .. } => {
            info!(%relays, "relays changed through the control socket");
        }
        Wanted::Code(Some(_)) => info!("a controller's code was given through the control socket"),
        Wanted::Code(None) => info!("back to the code the agent was started with, if any"),
    }
    let (answer, answered) = oneshot::channel();
    let change = Change { wanted, answer };
    agent
        .changes
        .send(change)
        .await
        .map_err(|_| "the agent is shutting down".to_string())?;
    match tokio::time::timeout(CHANGE_TIMEOUT, answered).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => Err("the agent is shutting down".into()),
        // The run loop skips changes whose asker is gone.
        Err(_) => Err("the agent is busy; try again".into()),
    }
}

/// The relays chosen earlier during this boot, from [`CHOSEN`].
pub fn chosen(text: &str) -> Result<Relays> {
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    match lines.as_slice() {
        [line] if line == "off" => Ok(Relays::Disabled),
        [line] if line == "n0" => Ok(Relays::N0),
        urls => Relays::from_args(urls, false),
    }
}

/// Connects to the agent whose state directory is `state_dir`.
pub async fn connect(state_dir: &Path) -> Result<UnixStream> {
    let path = state_dir.join(SOCKET);
    UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connecting to the agent at {} (is it running, and are you root?)",
            path.display()
        )
    })
}

/// Asks the agent to use other relays; returns its answer.
pub async fn set_relays(state_dir: &Path, setting: Setting) -> Result<String> {
    ask(state_dir, &Request::Relays(setting)).await
}

/// Asks the agent to wait for the controller with `code` (`None`: as it was
/// started); returns its answer.
pub async fn set_code(state_dir: &Path, code: Option<String>) -> Result<String> {
    ask(state_dir, &Request::Code(code)).await
}

async fn ask(state_dir: &Path, request: &Request) -> Result<String> {
    let mut stream = connect(state_dir).await?;
    write_frame(&mut stream, request).await?;
    match read_frame(&mut stream, MAX_FRAME)
        .await
        .context("reading the agent's answer")?
    {
        Reply::Done(done) => Ok(done),
        Reply::Refused(why) => bail!("{why}"),
        Reply::Status(_) => bail!("the agent answered with something else"),
    }
}

/// The agent's screen as it changes, from a connection [`watch`] opened.
pub struct Watching {
    stream: UnixStream,
}

impl Watching {
    /// The next state of the screen.
    pub async fn next(&mut self) -> Result<Status> {
        match read_frame(&mut self.stream, MAX_FRAME).await? {
            Reply::Status(status) => Ok(status),
            _ => bail!("the agent answered with something else"),
        }
    }
}

/// Follows the agent's screen.
pub async fn watch(state_dir: &Path) -> Result<Watching> {
    let mut stream = connect(state_dir).await?;
    write_frame(&mut stream, &Request::Watch).await?;
    Ok(Watching { stream })
}
