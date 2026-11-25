//! Running a program on the target with its output going to the controller
//! while it runs: disko and `nixos-install` as `Output` frames, programs of
//! hooks as `ExecOutput` frames.

use std::process::Stdio;

use anyhow::{Context, Result};
use nix_pixie_proto::ctrl::TargetMsg;
use nix_pixie_proto::frame::write_frame;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tracing::{info, trace};

use crate::net::Stream;

/// Runs `command` (called `what` in messages) without input. The outer
/// `Result` is about the control stream; the inner one is the program's
/// outcome, as a reason for the operator.
pub async fn run(
    what: &str,
    mut command: Command,
    stream: &mut Stream,
) -> Result<Result<(), String>> {
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Ok(Err(format!("starting {what}: {e}"))),
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Ok(Err(format!("{what}'s output is not connected")));
    };
    forward(stream, stdout, stderr, |_, chunk| TargetMsg::Output {
        chunk,
    })
    .await;
    let status = child
        .wait()
        .await
        .with_context(|| format!("waiting for {what}"))?;
    info!(what, %status, "finished");
    Ok(if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    })
}

/// Runs a hook's program (see [`crate::exec`]), its stdout and stderr
/// going as `ExecOutput` frames. Returns the frame that ends it.
pub async fn exec(mut command: Command, stream: &mut Stream) -> Result<TargetMsg> {
    let mut child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return Ok(TargetMsg::ExecFailed {
                error: format!("starting it: {e}"),
            });
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Ok(TargetMsg::ExecFailed {
            error: "its output is not connected".into(),
        });
    };
    forward(stream, stdout, stderr, |stderr, chunk| {
        TargetMsg::ExecOutput { stderr, chunk }
    })
    .await;
    let status = child.wait().await.context("waiting for a hook's program")?;
    info!(%status, "hook's program finished");
    Ok(TargetMsg::ExecExited {
        code: status.code(),
    })
}

/// Sends both pipes' data as frames until both are closed. `frame` makes
/// one from a chunk and whether it is from stderr. If the connection
/// breaks, the program goes on and its output is dropped: the controller
/// comes back for the result, which nobody should have to redo.
async fn forward(
    stream: &mut Stream,
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    frame: impl Fn(bool, Vec<u8>) -> TargetMsg,
) {
    let mut connected = true;
    let mut out_buf = vec![0; 16 * 1024];
    let mut err_buf = vec![0; 16 * 1024];
    let (mut out_open, mut err_open) = (true, true);
    loop {
        // `read` is cancel-safe: a branch that loses the race loses no data.
        // A read error counts as the end of that pipe.
        let (n, from_stdout) = tokio::select! {
            r = stdout.read(&mut out_buf), if out_open => (r.unwrap_or(0), true),
            r = stderr.read(&mut err_buf), if err_open => (r.unwrap_or(0), false),
            else => return,
        };
        if n == 0 {
            if from_stdout {
                out_open = false;
            } else {
                err_open = false;
            }
            continue;
        }
        trace!(bytes = n, "output");
        let buf = if from_stdout { &out_buf } else { &err_buf };
        let chunk = buf.get(..n).unwrap_or_default().to_vec();
        if connected
            && write_frame(stream, &frame(!from_stdout, chunk))
                .await
                .is_err()
        {
            connected = false;
        }
    }
}
