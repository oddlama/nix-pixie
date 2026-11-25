//! `nix-pixie target exec`: hooks run programs on the target through the
//! controller.
//!
//! While a hook runs, the controller listens on a socket in the hook's
//! private directory (`$PIXIE_TARGET`), and `"$PIXIE" target exec -- …`
//! sends it one [`Request`]. The controller pushes the program's closure to
//! the target, has it run there (see [`crate::exec`]), and passes on its
//! output and exit code, so a hook can use it like any local command.

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use nix_pixie_proto::ctrl::{ControllerMsg, StreamRequest, TargetMsg};
use nix_pixie_proto::frame::{read_frame, write_frame};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tracing::{debug, info};

use crate::exec;
use crate::link::Link;
use crate::net::sanitise;
use crate::push;

/// Where a hook finds the controller's socket.
pub const SOCKET_ENV: &str = "PIXIE_TARGET";
/// Upper bound for frames between a hook and the controller.
const MAX_FRAME: usize = 1024 * 1024;

/// What a hook asks the controller to run on the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// A file in the Nix store.
    pub program: String,
    pub args: Vec<String>,
    /// Inside the installed system rather than on the installer.
    pub chroot: bool,
}

/// What the controller tells the hook while and after the program runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Output {
        stderr: bool,
        chunk: Vec<u8>,
    },
    /// Its exit code; `None` if a signal ended it.
    Exited {
        code: Option<i32>,
    },
    /// Why it didn't run.
    Failed {
        error: String,
    },
}

/// The paired target, for hooks' programs while there is one.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub link: &'a Link,
    /// Where the installed system is mounted, once it is installed: for
    /// `--chroot`.
    pub installed: Option<&'a str>,
}

/// The `nix-pixie target exec` command: runs `program` with `args` on the
/// target of the install whose hook calls it, and exits like it.
pub async fn exec(program: String, args: Vec<String>, chroot: bool) -> Result<ExitCode> {
    let socket = std::env::var_os(SOCKET_ENV).with_context(|| {
        format!(
            "${SOCKET_ENV} isn't set: `nix-pixie target exec` is for hooks, while the \
             controller has a target (from `approve` until `finish`)"
        )
    })?;
    exec::store_path(&program).map_err(|e| anyhow!(e))?;
    let mut controller = UnixStream::connect(&socket)
        .await
        .context("connecting to the controller")?;
    let request = Request {
        program,
        args,
        chroot,
    };
    write_frame(&mut controller, &request).await?;
    let (mut stdout, mut stderr) = (tokio::io::stdout(), tokio::io::stderr());
    loop {
        match read_frame(&mut controller, MAX_FRAME)
            .await
            .context("the controller hung up")?
        {
            Event::Output {
                stderr: false,
                chunk,
            } => {
                stdout.write_all(&chunk).await?;
                stdout.flush().await?;
            }
            Event::Output {
                stderr: true,
                chunk,
            } => {
                stderr.write_all(&chunk).await?;
                stderr.flush().await?;
            }
            Event::Exited { code: Some(code) } => {
                return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
            }
            Event::Exited { code: None } => bail!("{} was killed by a signal", request.program),
            Event::Failed { error } => bail!("{error}"),
        }
    }
}

/// Serves one request of a hook: the program's closure goes to the target,
/// the program runs there, its output and exit code come back. A request
/// that can't be served is answered with why; errors are only returned if
/// the session with the target broke.
pub async fn serve(mut hook: UnixStream, target: Target<'_>) -> Result<()> {
    let request: Request = match read_frame(&mut hook, MAX_FRAME).await {
        Ok(request) => request,
        Err(e) => {
            debug!("a hook's request: {e:#}");
            return Ok(());
        }
    };
    let root = match (request.chroot, target.installed) {
        (false, _) => None,
        (true, Some(root)) => Some(root.to_string()),
        (true, None) => {
            let error = "--chroot needs the installed system: order the hook after `install`";
            return answer(
                &mut hook,
                Event::Failed {
                    error: error.into(),
                },
            )
            .await;
        }
    };
    if let Err(error) = push_program(target.link, &request.program, root.as_deref()).await {
        let error = format!("{error:#}");
        return answer(&mut hook, Event::Failed { error }).await;
    }
    info!(program = %request.program, chroot = request.chroot, "running a hook's program on the target");
    let message = ControllerMsg::Exec {
        program: request.program,
        args: request.args,
        root,
    };
    // Read to the end even if the hook is gone, to keep the session in step.
    let mut listening = true;
    let reply = target
        .link
        .request_with_output(&message, async |frame| {
            if let TargetMsg::ExecOutput { stderr, chunk } = frame
                && listening
                && write_frame(&mut hook, &Event::Output { stderr, chunk })
                    .await
                    .is_err()
            {
                listening = false;
            }
        })
        .await?;
    let event = match reply {
        TargetMsg::ExecExited { code } => Event::Exited { code },
        TargetMsg::ExecFailed { error } => Event::Failed {
            error: sanitise(&error, 600),
        },
        _ => bail!("unexpected reply to running a hook's program"),
    };
    answer(&mut hook, event).await
}

/// Copies the store path `program` is in, with its closure, to where the
/// target runs it: its live store, or the store under `root`.
async fn push_program(link: &Link, program: &str, root: Option<&str>) -> Result<()> {
    let path = exec::store_path(program).map_err(|e| anyhow!(e))?;
    push::check_local_paths(&[path.to_string()]).await?;
    let request = match root {
        None => StreamRequest::LiveStore,
        Some(root) => StreamRequest::Store { root: root.into() },
    };
    push::send(link, &request, &[path.to_string()])
        .await
        .with_context(|| format!("copying {path} to the target"))?;
    Ok(())
}

/// Tells the hook why its request isn't served, if it still listens.
async fn answer(hook: &mut UnixStream, event: Event) -> Result<()> {
    let _ = write_frame(hook, &event).await;
    Ok(())
}
