//! Handing a controller's code to a target over a serial port
//! (`--serial`): its status terminal (`nix-pixie console --plain` or
//! `--json`, e.g. on a USB gadget's serial port) takes `code CODE`.
//!
//! The port may not be there yet (the device is still booting), may go
//! away and come back (the host enumerates it anew), and drops what is
//! written while nobody reads at the other end. So the code goes again
//! until the target answers it.
//!
//! The port is ours alone while we use it (`TIOCEXCL`): two readers split
//! what arrives between them, and each misses what the other got.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix_pixie_proto::code::Code;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};
use rustix::termios::{self, ControlModes, OptionalActions};
use tracing::{debug, info, warn};

/// How long to wait for the target's answer before writing again.
const ANSWER_WAIT: Duration = Duration::from_secs(3);
/// The pause before looking for a port that isn't there.
const PORT_WAIT: Duration = Duration::from_millis(500);

/// Hands `code` to the target at `port` until it answers. Abort the task
/// once the target paired, or pairing failed.
pub fn spawn(port: PathBuf, code: Code) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (mut waiting, mut shared) = (false, false);
        loop {
            let attempt = {
                let port = port.clone();
                tokio::task::spawn_blocking(move || hand_over(&port, code)).await
            };
            match attempt {
                Ok(Ok(answer)) => {
                    let answer = crate::net::sanitise(&answer, 200);
                    info!(port = %port.display(), %answer, "handed the code over the serial port");
                    return;
                }
                Ok(Err(e)) if e.is::<Shared>() => {
                    if !shared {
                        warn!(port = %port.display(), "{e:#}; waiting for it to be ours alone");
                        shared = true;
                    }
                    tokio::time::sleep(PORT_WAIT).await;
                }
                Ok(Err(e)) if !port.exists() => {
                    if !waiting {
                        info!(port = %port.display(), "waiting for the serial port to show up");
                        waiting = true;
                    }
                    debug!("serial port: {e:#}");
                    tokio::time::sleep(PORT_WAIT).await;
                }
                Ok(Err(e)) => {
                    debug!(port = %port.display(), "{e:#}");
                    tokio::time::sleep(PORT_WAIT).await;
                }
                Err(e) => {
                    warn!("handing over the code: {e}");
                    return;
                }
            }
        }
    })
}

/// Opens the port and writes the code until an answer comes, which a
/// port that is closed would drop. Fails when the port does.
fn hand_over(port: &Path, code: Code) -> Result<String> {
    let mut file = open(port)?;
    let mut pending = Vec::new();
    loop {
        // An empty line first ends whatever was half typed there, and has
        // the target write its status again.
        write_all(&mut file, format!("\r\ncode {code}\r\n").as_bytes())?;
        if let Some(answer) = await_answer(&mut file, &mut pending)? {
            return Ok(answer);
        }
        debug!(port = %port.display(), "no answer within {} s; again", ANSWER_WAIT.as_secs());
    }
}

/// The target's answer to `code`, if it comes within [`ANSWER_WAIT`].
fn await_answer(file: &mut File, pending: &mut Vec<u8>) -> Result<Option<String>> {
    let deadline = Instant::now() + ANSWER_WAIT;
    let mut buf = [0; 1024];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if !ready(file, PollFlags::IN, left)? {
            break;
        }
        let n = match file.read(&mut buf) {
            Ok(0) => bail!("the port closed"),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e).context("reading the serial port"),
        };
        pending.extend_from_slice(buf.get(..n).unwrap_or_default());
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            debug!(%line, "target's status terminal");
            if let Some(answer) = answer(line) {
                return Ok(Some(answer));
            }
        }
    }
    Ok(None)
}

/// The port, raw, ignoring the modem's carrier line: a USB gadget's
/// doesn't report one reliably. Never our controlling terminal.
fn open(port: &Path) -> Result<File> {
    let meta = std::fs::metadata(port).with_context(|| format!("{}", port.display()))?;
    // Opening a path that isn't there yet, or a device's symlink udev hasn't
    // made, mustn't create a file instead.
    if !meta.file_type().is_char_device() {
        bail!("{} is not a serial port (yet)", port.display());
    }
    let fd = rustix::fs::open(
        port,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening {}", port.display()))?;
    let file = File::from(fd);
    // Nobody opens it after us (but root), then nobody else may have it.
    termios::ioctl_tiocexcl(file.as_fd())
        .with_context(|| format!("making {} ours alone", port.display()))?;
    let others = openers(meta.rdev());
    if !others.is_empty() {
        return Err(Shared {
            port: port.to_path_buf(),
            others,
        }
        .into());
    }
    let mut settings = termios::tcgetattr(file.as_fd())
        .with_context(|| format!("reading the settings of {}", port.display()))?;
    settings.make_raw();
    settings.control_modes |= ControlModes::CLOCAL | ControlModes::CREAD;
    termios::tcsetattr(file.as_fd(), OptionalActions::Now, &settings)
        .with_context(|| format!("setting up {}", port.display()))?;
    Ok(file)
}

/// Another process has the port open: it would read some of the answer.
#[derive(Debug, thiserror::Error)]
#[error(
    "{} is open in {} as well, which would read some of what the target writes",
    port.display(),
    others.join(", ")
)]
struct Shared {
    port: PathBuf,
    others: Vec<String>,
}

/// Other processes that have the terminal `rdev` open (`pid (name)`), of
/// those this one may look at: its user's, or all of them as root.
#[must_use]
pub fn openers(rdev: u64) -> Vec<String> {
    let me = std::process::id().to_string();
    let mut found = Vec::new();
    for process in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = process.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        let has_it = fds.flatten().any(|fd| {
            std::fs::metadata(fd.path())
                .is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == rdev)
        });
        if has_it {
            let name = std::fs::read_to_string(process.path().join("comm")).unwrap_or_default();
            found.push(format!("{pid} ({})", name.trim()));
        }
    }
    found
}

fn write_all(file: &mut File, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        if !ready(file, PollFlags::OUT, ANSWER_WAIT)? {
            bail!("the serial port takes nothing");
        }
        match file.write(bytes) {
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("writing to the serial port"),
        }
    }
    Ok(())
}

/// Whether `file` is ready for `flags` within `wait`.
fn ready(file: &File, flags: PollFlags, wait: Duration) -> Result<bool> {
    let timeout = Timespec::try_from(wait).ok();
    let mut fds = [PollFd::new(file, flags)];
    let n = poll(&mut fds, timeout.as_ref()).context("waiting for the serial port")?;
    let revents = fds.first().map_or(PollFlags::empty(), PollFd::revents);
    if revents.intersects(PollFlags::HUP | PollFlags::ERR) && !revents.contains(PollFlags::IN) {
        bail!("the serial port hung up");
    }
    Ok(n > 0)
}

/// The target's answer to `code`, from a line of its status terminal:
/// `> code: text` (`--plain`), or an `answer` event for it (`--json`).
/// Answers to anything else are someone else's.
#[must_use]
pub fn answer(line: &str) -> Option<String> {
    if let Some(text) = line.strip_prefix("> code: ") {
        return Some(text.to_string());
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("event")?.as_str()? != "answer" || value.get("command")?.as_str()? != "code" {
        return None;
    }
    Some(value.get("text")?.as_str()?.to_string())
}
