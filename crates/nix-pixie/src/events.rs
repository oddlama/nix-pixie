//! Machine-readable progress for a program that drives the controller, such
//! as a wrapper that boots devices over USB and hands them to nix-pixie:
//! with `--events FILE`, one JSON object per line, as things happen.
//!
//! Every event has `v` (this format's version, [`VERSION`]), `time` (Unix
//! time in milliseconds) and `event`, one of:
//!
//! - `steps`: the install's steps, `[{key, title}]`, whenever they change.
//! - `step`: `key`, `state` (`started`, `done`, `skipped`, `failed`) and a
//!   `detail` for the operator.
//! - `progress`: Nix's progress in the running step `key`: `bytes` and
//!   `bytesExpected`, `paths` and `pathsExpected` copied, `builds` and
//!   `buildsExpected`. At most once a second.
//! - `notice`: `lines` told to the operator.
//! - `question`: `text` asked on the terminal; nothing goes on until it is
//!   answered. With `--yes`, `--wipe` and the like there are none.
//! - `result`: the `result` printed on stdout.
//! - `exit`: the process ends with `code` (see [`crate::exit`]), the
//!   `phase` it had reached, and the `error`, if any.
//!
//! Fields may be added within a version; a version change means a field
//! changed or went away.

use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The format's version, `v` in every event.
pub const VERSION: u32 = 1;

fn sink() -> &'static Mutex<Option<std::fs::File>> {
    static SINK: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

/// Sends events to `path`: a file (appended to), a FIFO, or `/dev/fd/N`.
pub fn open(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for events", path.display()))?;
    if let Ok(mut sink) = sink().lock() {
        *sink = Some(file);
    }
    Ok(())
}

/// Whether events are wanted.
pub fn enabled() -> bool {
    sink().lock().is_ok_and(|sink| sink.is_some())
}

fn emit(event: &str, mut fields: Value) {
    let Ok(mut sink) = sink().lock() else {
        return;
    };
    let Some(file) = sink.as_mut() else {
        return;
    };
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    if let Some(object) = fields.as_object_mut() {
        object.insert("v".into(), VERSION.into());
        object.insert("time".into(), time.into());
        object.insert("event".into(), event.into());
    }
    let mut line = fields.to_string();
    line.push('\n');
    // A reader that went away mustn't stop the install.
    if file.write_all(line.as_bytes()).is_err() {
        *sink = None;
    }
}

/// The steps, as `(key, title)`.
pub fn steps(list: &[(String, String)]) {
    let steps: Vec<Value> = list
        .iter()
        .map(|(key, title)| json!({ "key": key, "title": title }))
        .collect();
    emit("steps", json!({ "steps": steps }));
}

/// What happened to a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Started,
    Done,
    Skipped,
    Failed,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Done => "done",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// A step changed.
pub fn step(key: &str, state: State, detail: &str) {
    emit(
        "step",
        json!({ "key": key, "state": state.name(), "detail": detail }),
    );
}

/// Nix's progress in the running step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub bytes: u64,
    pub bytes_expected: u64,
    pub paths: u64,
    pub paths_expected: u64,
    pub builds: u64,
    pub builds_expected: u64,
}

/// Progress in the step `key` (`None`: whichever runs).
pub fn progress(key: Option<&str>, progress: Progress) {
    emit(
        "progress",
        json!({
            "key": key,
            "bytes": progress.bytes,
            "bytesExpected": progress.bytes_expected,
            "paths": progress.paths,
            "pathsExpected": progress.paths_expected,
            "builds": progress.builds,
            "buildsExpected": progress.builds_expected,
        }),
    );
}

/// Lines told to the operator.
pub fn notice(lines: &[String]) {
    if !lines.is_empty() {
        emit("notice", json!({ "lines": lines }));
    }
}

/// A question waits for an answer on the terminal.
pub fn question(text: &str) {
    emit("question", json!({ "text": text }));
}

/// The result, as printed on stdout.
pub fn result(line: &str) {
    let result = serde_json::from_str::<Value>(line).unwrap_or_else(|_| line.into());
    emit("result", json!({ "result": result }));
}

/// The process ends.
pub fn exit(code: u8, phase: &str, error: Option<&str>) {
    emit(
        "exit",
        json!({ "code": code, "phase": phase, "error": error }),
    );
}
