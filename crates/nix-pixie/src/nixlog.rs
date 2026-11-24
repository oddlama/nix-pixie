//! Nix's progress, shown on the step that runs it.
//!
//! On a live console, `nix build` and `nix copy` run with `--log-format
//! internal-json`: a line `@nix {…}` for every activity Nix starts or stops,
//! for its progress, and for every message. They add up to a one-line
//! summary for the running step, much like Nix's own progress bar
//! (`3/17 built · 120/400 copied · 1.2/3.4 GiB · building linux-6.12`).
//! Warnings become log lines, errors are printed as Nix wrote them, and
//! everything else is logged at debug level. In plain mode Nix writes to
//! stderr itself, as it always does.

use std::collections::HashMap;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tracing::{debug, trace, warn};

use crate::controller::human_size;
use crate::ui;

/// Activity types, `ActivityType` in Nix's `logging.hh`.
mod kind {
    pub const COPY_PATH: u64 = 100;
    pub const COPY_PATHS: u64 = 103;
    pub const BUILDS: u64 = 104;
    pub const BUILD: u64 = 105;
    pub const SUBSTITUTE: u64 = 108;
}

/// Result types, `ResultType` in Nix's `logging.hh`.
mod result {
    pub const BUILD_LOG_LINE: u64 = 101;
    pub const PROGRESS: u64 = 105;
    pub const SET_EXPECTED: u64 = 106;
}

/// Verbosity levels, `Verbosity` in Nix's `logging.hh`.
mod level {
    pub const ERROR: u64 = 0;
    pub const WARN: u64 = 1;
    pub const TALKATIVE: u64 = 4;
}

/// How often a running step's summary is updated at most.
const UPDATE_EVERY: Duration = Duration::from_millis(50);

/// One line of `internal-json` output.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Event {
    Start {
        id: u64,
        #[serde(default)]
        level: u64,
        #[serde(rename = "type", default)]
        kind: u64,
        #[serde(default)]
        text: String,
        #[serde(default)]
        fields: Vec<serde_json::Value>,
    },
    Stop {
        id: u64,
    },
    Result {
        id: u64,
        #[serde(rename = "type")]
        kind: u64,
        #[serde(default)]
        fields: Vec<serde_json::Value>,
    },
    Msg {
        level: u64,
        msg: String,
    },
    /// Actions this doesn't know (`setPhase` and friends come as results).
    #[serde(other)]
    Other,
}

/// A line Nix wrote to stderr.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Event(Event),
    /// Anything else, e.g. from a remote builder's SSH.
    Text(String),
}

/// Reads one line of Nix's stderr.
pub fn parse(line: &str) -> Line {
    line.strip_prefix("@nix ")
        .and_then(|json| serde_json::from_str(json).ok())
        .map_or_else(|| Line::Text(line.to_string()), Line::Event)
}

/// What Nix is doing, from its activities.
#[derive(Debug, Default)]
pub struct Progress {
    running: HashMap<u64, Activity>,
    /// Per activity type: what finished activities did, and what is
    /// expected of the type (`setExpected`).
    totals: HashMap<u64, Totals>,
    /// Running activities worth naming, the newest last.
    named: Vec<u64>,
}

/// A running activity.
#[derive(Debug, Default)]
struct Activity {
    kind: u64,
    /// What to call it, if it is worth naming.
    name: Option<String>,
    done: u64,
    expected: u64,
    running: u64,
    failed: u64,
    /// What it said to expect of other types, e.g. a copy's bytes.
    expects: HashMap<u64, u64>,
}

/// Sums for one activity type.
#[derive(Debug, Default, Clone, Copy)]
struct Totals {
    done: u64,
    failed: u64,
    expected: u64,
}

/// Progress counts for one activity type, like Nix's bar adds them up.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub done: u64,
    pub expected: u64,
    pub running: u64,
    pub failed: u64,
}

fn number(fields: &[serde_json::Value], i: usize) -> u64 {
    fields
        .get(i)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn string(fields: &[serde_json::Value], i: usize) -> Option<&str> {
    fields.get(i).and_then(serde_json::Value::as_str)
}

impl Progress {
    pub fn feed(&mut self, event: &Event) {
        match event {
            Event::Start {
                id,
                level,
                kind,
                text,
                fields,
            } => {
                let name = describe(*kind, *level, text, fields);
                if name.is_some() {
                    self.named.push(*id);
                }
                self.running.insert(
                    *id,
                    Activity {
                        kind: *kind,
                        name,
                        ..Activity::default()
                    },
                );
            }
            Event::Stop { id } => {
                let Some(activity) = self.running.remove(id) else {
                    return;
                };
                self.named.retain(|named| named != id);
                let totals = self.totals.entry(activity.kind).or_default();
                totals.done += activity.done;
                totals.failed += activity.failed;
                for (kind, n) in activity.expects {
                    let totals = self.totals.entry(kind).or_default();
                    totals.expected = totals.expected.saturating_sub(n);
                }
            }
            Event::Result { id, kind, fields } => {
                let Some(activity) = self.running.get_mut(id) else {
                    return;
                };
                match *kind {
                    result::PROGRESS => {
                        activity.done = number(fields, 0);
                        activity.expected = number(fields, 1);
                        activity.running = number(fields, 2);
                        activity.failed = number(fields, 3);
                    }
                    result::SET_EXPECTED => {
                        let (kind, n) = (number(fields, 0), number(fields, 1));
                        let old = activity.expects.insert(kind, n).unwrap_or(0);
                        let totals = self.totals.entry(kind).or_default();
                        totals.expected = totals.expected.saturating_sub(old) + n;
                    }
                    _ => {}
                }
            }
            Event::Msg { .. } | Event::Other => {}
        }
    }

    /// What activities of `kind` did and expect, running and finished.
    pub fn count(&self, kind: u64) -> Count {
        let totals = self.totals.get(&kind).copied().unwrap_or_default();
        let mut count = Count {
            done: totals.done,
            expected: totals.done,
            running: 0,
            failed: totals.failed,
        };
        for activity in self.running.values().filter(|a| a.kind == kind) {
            count.done += activity.done;
            count.expected += activity.expected;
            count.running += activity.running;
            count.failed += activity.failed;
        }
        count.expected = count.expected.max(totals.expected);
        count
    }

    /// Derivations built, of how many.
    pub fn builds(&self) -> Count {
        self.count(kind::BUILDS)
    }

    /// Store paths copied (from a cache, or to the target), of how many.
    pub fn copies(&self) -> Count {
        self.count(kind::COPY_PATHS)
    }

    /// Bytes copied, of how many.
    pub fn bytes(&self) -> Count {
        self.count(kind::COPY_PATH)
    }

    /// The newest running activity worth naming.
    pub fn current(&self) -> Option<&str> {
        self.named
            .last()
            .and_then(|id| self.running.get(id))
            .and_then(|a| a.name.as_deref())
    }

    /// One line on where Nix is, empty while there is nothing to say.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let builds = self.builds();
        if builds.expected > 0 {
            parts.push(format!("{}/{} built", builds.done, builds.expected));
        }
        let copies = self.copies();
        if copies.expected > 0 {
            parts.push(format!("{}/{} copied", copies.done, copies.expected));
        }
        let bytes = self.bytes();
        if bytes.expected > 0 {
            parts.push(format!(
                "{}/{}",
                human_size(bytes.done),
                human_size(bytes.expected)
            ));
        }
        if builds.failed > 0 {
            parts.push(format!("{} failed", builds.failed));
        }
        parts.extend(self.current().map(str::to_string));
        parts.join(" · ")
    }

    /// What was done, for when it is over: empty if nothing was.
    pub fn outcome(&self) -> String {
        let mut parts = Vec::new();
        let builds = self.builds();
        if builds.done > 0 {
            parts.push(format!("{} built", builds.done));
        }
        let copies = self.copies();
        if copies.done > 0 {
            parts.push(format!("{} copied", copies.done));
        }
        let bytes = self.bytes();
        if bytes.done > 0 {
            parts.push(human_size(bytes.done));
        }
        parts.join(" · ")
    }
}

/// What to call an activity in the summary, if anything: what is being
/// built or copied, and evaluation (which can take a while before anything
/// else happens).
fn describe(kind: u64, level: u64, text: &str, fields: &[serde_json::Value]) -> Option<String> {
    match kind {
        kind::BUILD => Some(format!("building {}", text_or(fields, 0, text, store_name))),
        kind::SUBSTITUTE => Some(format!("fetching {}", text_or(fields, 0, text, store_name))),
        kind::COPY_PATH => {
            let verb = if text.contains(" from ") {
                "fetching"
            } else {
                "copying"
            };
            Some(format!("{verb} {}", text_or(fields, 0, text, store_name)))
        }
        0 if level <= level::TALKATIVE && text.starts_with("evaluating") => {
            Some("evaluating".to_string())
        }
        _ => None,
    }
}

/// `f` of field `i` if it is a string, else `text` with its store paths
/// shortened.
fn text_or(fields: &[serde_json::Value], i: usize, text: &str, f: impl Fn(&str) -> &str) -> String {
    string(fields, i).map_or_else(|| shorten_store_paths(text), |field| f(field).to_string())
}

/// The name in a store path: `/nix/store/<hash>-glibc-2.40.drv` is
/// `glibc-2.40`.
pub fn store_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    let name = base.split_once('-').map_or(base, |(_, name)| name);
    name.strip_suffix(".drv").unwrap_or(name)
}

/// `text` with every quoted store path replaced by its name.
pub fn shorten_store_paths(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("/nix/store/") {
        let (before, path) = rest.split_at(start);
        out.push_str(before);
        let end = path
            .find(|c: char| c == '\'' || c == '"' || c.is_whitespace())
            .unwrap_or(path.len());
        let (path, after) = path.split_at(end);
        out.push_str(store_name(path));
        rest = after;
    }
    out.push_str(rest);
    out
}

/// `text` without terminal escape sequences.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// What a finished `nix` run left.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// What it did (empty in plain mode).
    pub progress: Progress,
}

/// Runs `nix` without input, keeping its stdout. On a live console its
/// progress goes into the running step (see the module documentation);
/// otherwise its stderr is ours.
pub async fn run(mut command: tokio::process::Command) -> Result<Output> {
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    if !ui::live() {
        let output = command
            .stderr(Stdio::inherit())
            .output()
            .await
            .context("running `nix` (is Nix installed?)")?;
        return Ok(Output {
            status: output.status,
            stdout: output.stdout,
            progress: Progress::default(),
        });
    }
    let mut child = command
        .args(["--log-format", "internal-json"])
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("running `nix` (is Nix installed?)")?;
    let (Some(mut stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        anyhow::bail!("`nix`'s output is not connected");
    };
    let collect = async {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.map(|_| out)
    };
    let (stdout, progress) =
        tokio::try_join!(collect, follow(stderr)).context("reading what `nix` wrote")?;
    let status = child.wait().await.context("waiting for `nix`")?;
    Ok(Output {
        status,
        stdout,
        progress,
    })
}

/// Reads Nix's stderr to its end, passing on what it says and its progress.
async fn follow(stderr: tokio::process::ChildStderr) -> std::io::Result<Progress> {
    let mut progress = Progress::default();
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    let mut updated: Option<Instant> = None;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).await? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        match parse(line) {
            Line::Event(Event::Msg { level, msg }) => say(level, &msg),
            Line::Event(Event::Result {
                kind: result::BUILD_LOG_LINE,
                fields,
                ..
            }) => trace!(line = string(&fields, 0).unwrap_or_default(), "build log"),
            Line::Event(event) => {
                progress.feed(&event);
                if updated.is_none_or(|t| t.elapsed() >= UPDATE_EVERY) {
                    ui::progress(progress.summary());
                    updated = Some(Instant::now());
                }
            }
            Line::Text(text) if text.trim().is_empty() => {}
            Line::Text(text) => ui::note(std::slice::from_ref(&text)),
        }
    }
    ui::progress(progress.summary());
    Ok(progress)
}

/// Passes on a message from Nix: errors as Nix wrote them (the failure
/// names `nix`, and this is why), warnings as warnings, the rest as debug.
fn say(level: u64, msg: &str) {
    match level {
        level::ERROR => {
            let msg = if ui::colors() {
                msg.to_string()
            } else {
                strip_ansi(msg)
            };
            ui::note(&[msg]);
        }
        level::WARN => {
            let msg = strip_ansi(msg);
            let msg = msg.strip_prefix("warning: ").unwrap_or(&msg);
            warn!("nix: {msg}");
        }
        _ => debug!("nix: {}", strip_ansi(msg)),
    }
}
