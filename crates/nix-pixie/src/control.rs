//! The agent's control socket, `control.sock` in its state directory,
//! which only root can enter. `nix-pixie relay` changes the agent's relays
//! through it, and `nix-pixie console` follows its [`Status`].
//!
//! Frames as everywhere else (see `nix_pixie_proto::frame`), one request per
//! connection.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
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
/// Upper bound for a frame on the socket.
const MAX_FRAME: usize = 256 * 1024;
/// How long a relay change may wait for the agent to take it.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a client asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The screen, now and whenever it changes.
    Watch,
    /// Use other relays.
    Relays(Setting),
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

/// New relays for the agent's run loop, and where its answer goes.
pub struct Change {
    pub relays: Relays,
    /// For [`CHOSEN`]; `None` to go back to the configured relays.
    pub chosen: Option<String>,
    pub answer: oneshot::Sender<Result<String, String>>,
}

/// What the agent's run loop shares with the socket.
pub struct Agent {
    /// The relays it was started with.
    pub configured: Relays,
    /// Changes go here, while [`Self::idle`].
    pub changes: mpsc::Sender<Change>,
    /// Whether it waits for a controller, rather than pairing or serving
    /// one: only then may the relays change.
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
            let reply = match change(agent, setting).await {
                Ok(done) => Reply::Done(done),
                Err(refused) => Reply::Refused(refused),
            };
            write_frame(&mut stream, &reply).await?;
            Ok(())
        }
    }
}

/// Hands new relays to the run loop, if it can take them now.
async fn change(agent: &Agent, setting: Setting) -> Result<String, String> {
    let (relays, chosen) = match &setting {
        Setting::Configured => (agent.configured.clone(), None),
        Setting::Off => (Relays::Disabled, Some("off".to_string())),
        Setting::N0 => (Relays::N0, Some("n0".to_string())),
        Setting::Urls(urls) => (
            Relays::from_args(urls, false).map_err(|e| format!("{e:#}"))?,
            Some(urls.join("\n")),
        ),
    };
    if !agent.idle.load(Ordering::Relaxed) {
        return Err(
            "a controller is pairing or paired right now; change the relays once it is done".into(),
        );
    }
    info!(%relays, "relays changed through the control socket");
    let (answer, answered) = oneshot::channel();
    let change = Change {
        relays,
        chosen,
        answer,
    };
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
    let mut stream = connect(state_dir).await?;
    write_frame(&mut stream, &Request::Relays(setting)).await?;
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
