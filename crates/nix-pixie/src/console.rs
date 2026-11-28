//! The installer's tty1 (`nix-pixie console`): the agent's code and how to
//! pair with it, its relay and its messages, and a prompt for commands such
//! as `relay URL` and `code CODE`. It talks to the agent over its control
//! socket (see [`crate::control`]), so either can restart without the other.
//!
//! The installer redirects kernel messages to tty12, so nothing scribbles
//! over the screen, and keeps a shell on tty2.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::termios::{self, OptionalActions, Termios};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthChar;

use crate::control::{self, Setting, Watching};
use crate::screen::Status;

/// What the operator is told about the other ttys and the commands.
pub const HINT: &str = "Alt+F2: a shell (tty2). Kernel messages: tty12. Commands: relay URL, relay off, \
     relay default, code CODE, help.";
const HELP: &str = "relay URL...: use your own relays (e.g. an iroh-relay) · relay n0: n0's \
                    public relays · relay off: none, only this network · relay default: as the \
                    agent was started · the agent then shows a new code · code CODE: wait for \
                    the controller with this code (three words) instead · code default: as the \
                    agent was started";

/// What the console shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// The agent's screen; `None` while it can't be reached.
    pub status: Option<Status>,
    /// The answer to the last command.
    pub notice: Option<String>,
    /// What is typed at the prompt.
    pub input: String,
}

/// Draws `view` on a `width` × `height` screen: the bytes to write, which
/// leave the cursor at the prompt.
#[must_use]
pub fn render(view: &View, width: usize, height: usize) -> String {
    let width = width.max(20);
    let mut top = vec![
        bold("nix-pixie installer"),
        "─".repeat(width.min(60)),
        String::new(),
    ];
    match &view.status {
        None => top.push(
            "Waiting for the agent (see `systemctl status nix-pixie-agent` on tty2)...".into(),
        ),
        Some(status) => {
            if let Some(shown) = &status.code {
                top.push(format!("Pairing code:  {}", bold(&shown.code)));
                top.push(String::new());
                top.push("On the controller:".into());
                top.push(format!("  {}", shown.command));
                top.push(String::new());
            }
            if let Some(relay) = &status.relay {
                top.push(relay.clone());
                top.push(String::new());
            }
        }
    }
    let mut bottom = vec!["─".repeat(width.min(60)), dim(HINT)];
    if let Some(notice) = &view.notice {
        bottom.push(notice.clone());
    }
    let wrap_all = |lines: &[String]| -> Vec<String> {
        lines.iter().flat_map(|line| wrap(line, width)).collect()
    };
    let top = wrap_all(&top);
    let bottom = wrap_all(&bottom);
    // The newest messages that fit between them, above the prompt line.
    let room = height.saturating_sub(top.len() + bottom.len() + 1);
    let messages: Vec<String> = view
        .status
        .as_ref()
        .map_or_else(Vec::new, |s| wrap_all(&s.messages));
    let shown = messages
        .get(messages.len().saturating_sub(room)..)
        .unwrap_or_default();

    let mut out = String::from("\x1b[H\x1b[2J");
    for line in top.iter().chain(shown).chain(&bottom) {
        out.push_str(line);
        out.push_str("\r\n");
    }
    // The end of the input, if it is too long for the line.
    let prompt = "> ";
    let room = width.saturating_sub(prompt.len() + 1);
    let skip = view.input.chars().count().saturating_sub(room);
    out.push_str(prompt);
    out.extend(view.input.chars().skip(skip));
    out
}

fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[0m")
}

fn dim(text: &str) -> String {
    format!("\x1b[2m{text}\x1b[0m")
}

/// `line` in pieces of at most `width` columns, broken at spaces where it
/// can be, each indented like the line. Escape sequences take no room.
#[must_use]
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    let text = line.trim_start_matches(' ');
    let indent = " ".repeat(line.len() - text.len());
    wrap_words(text, width.saturating_sub(indent.len()).max(1))
        .into_iter()
        .map(|piece| format!("{indent}{piece}"))
        .collect()
}

fn wrap_words(line: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for word in line.split(' ') {
        let len = columns(word);
        if used > 0 && used + 1 + len > width {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        if used > 0 {
            current.push(' ');
            used += 1;
        }
        if used + len <= width {
            current.push_str(word);
            used += len;
            continue;
        }
        // Too long for any line: cut, keeping escape sequences whole.
        let mut escape = false;
        for c in word.chars() {
            if escape || c == '\x1b' {
                escape = c == '\x1b' || !c.is_ascii_alphabetic();
                current.push(c);
                continue;
            }
            let w = c.width().unwrap_or(0);
            if used + w > width && used > 0 {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(c);
            used += w;
        }
    }
    out.push(current);
    out
}

/// How many columns `text` takes, without escape sequences.
fn columns(text: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    for c in text.chars() {
        if escape {
            escape = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            escape = true;
        } else {
            width += c.width().unwrap_or(0);
        }
    }
    width
}

/// What a line typed at the prompt does: the notice to show.
async fn command(state_dir: &Path, line: &str) -> String {
    let words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    match words.split_first() {
        None => String::new(),
        Some((first, _)) if first == "help" => HELP.into(),
        Some((first, rest)) if first == "relay" => {
            let result = match Setting::parse(rest) {
                Ok(setting) => control::set_relays(state_dir, setting).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(done) => done,
                Err(e) => format!("{e:#}"),
            }
        }
        Some((first, rest)) if first == "code" => {
            let result = match rest {
                [word] if word == "default" => control::set_code(state_dir, None).await,
                [code] => control::set_code(state_dir, Some(code.clone())).await,
                _ => Err(anyhow::anyhow!(
                    "which code? The controller's (three words), or `default`"
                )),
            };
            match result {
                Ok(done) => done,
                Err(e) => format!("{e:#}"),
            }
        }
        Some(_) => "no such command (help lists them)".into(),
    }
}

/// The terminal in raw mode until dropped: keys arrive one by one, and
/// nothing is echoed or turned into signals.
struct Raw {
    saved: Termios,
}

impl Raw {
    fn enter() -> Result<Self> {
        let stdin = std::io::stdin();
        let saved = termios::tcgetattr(&stdin).context("reading the terminal's settings")?;
        let mut raw = saved.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)
            .context("switching the terminal to raw mode")?;
        Ok(Self { saved })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin(), OptionalActions::Now, &self.saved);
    }
}

/// Keys as they are pressed, from a thread that reads stdin.
fn keys() -> mpsc::Receiver<u8> {
    let (send, keys) = mpsc::channel(256);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0; 64];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 {
                return;
            }
            for &byte in buf.get(..n).unwrap_or_default() {
                if send.blocking_send(byte).is_err() {
                    return;
                }
            }
        }
    });
    keys
}

/// Line editing at the prompt: what a key does.
#[derive(Debug, Default)]
struct Editor {
    /// In an escape sequence (arrow keys and the like), which are ignored.
    escape: Option<Vec<u8>>,
}

enum Key {
    Typed,
    Enter,
}

impl Editor {
    fn key(&mut self, byte: u8, input: &mut String) -> Option<Key> {
        if let Some(seq) = &mut self.escape {
            seq.push(byte);
            // ESC [ ... final byte, or ESC and one more.
            let done = match seq.as_slice() {
                [b'['] => false,
                [b'[', .., last] => (0x40..=0x7e).contains(last),
                _ => true,
            };
            if done {
                self.escape = None;
            }
            return None;
        }
        match byte {
            0x1b => self.escape = Some(Vec::new()),
            b'\r' | b'\n' => return Some(Key::Enter),
            0x7f | 0x08 => {
                input.pop();
            }
            // Ctrl-U
            0x15 => input.clear(),
            byte if byte.is_ascii_graphic() || byte == b' ' => input.push(char::from(byte)),
            _ => return None,
        }
        Some(Key::Typed)
    }
}

/// Runs the console until stdin ends.
pub async fn run(state_dir: PathBuf) -> Result<()> {
    let _raw = Raw::enter()?;
    let mut keys = keys();
    let mut editor = Editor::default();
    let mut view = View::default();
    let mut watching: Option<Watching> = None;
    loop {
        draw(&view)?;
        if watching.is_none() {
            watching = control::watch(&state_dir).await.ok();
            if watching.is_none() {
                view.status = None;
            }
        }
        tokio::select! {
            status = async {
                if let Some(watching) = &mut watching {
                    watching.next().await
                } else {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Err(anyhow::anyhow!("not connected"))
                }
            } => match status {
                Ok(status) => view.status = Some(status),
                Err(_) => watching = None,
            },
            key = keys.recv() => {
                let Some(key) = key else { return Ok(()) };
                if let Some(Key::Enter) = editor.key(key, &mut view.input) {
                    let line = std::mem::take(&mut view.input);
                    view.notice = Some(format!("> {line}: ..."));
                    draw(&view)?;
                    let answer = command(&state_dir, &line).await;
                    let word = line.split_whitespace().next().unwrap_or_default();
                    view.notice = (!answer.is_empty()).then(|| format!("{word}: {answer}"));
                }
            }
        }
    }
}

fn draw(view: &View) -> Result<()> {
    let stdout = std::io::stdout();
    let (width, height) = termios::tcgetwinsize(&stdout)
        .map(|size| (usize::from(size.ws_col), usize::from(size.ws_row)))
        .ok()
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or((80, 25));
    let mut out = stdout.lock();
    out.write_all(render(view, width, height).as_bytes())
        .and_then(|()| out.flush())
        .context("drawing the console")
}

/// How [`run_lines`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lines {
    /// For people: `code …`, `relay …`, the messages, and answers as
    /// `> COMMAND: …`.
    Plain,
    /// One JSON object per line, for programs: `{"v":1,"event":…}` with
    /// `status` (`code`, `command`, `relay`), `message` (`text`) and
    /// `answer` (`command`, `text`) events.
    Json,
}

/// How many written lines [`run_lines`] remembers, to know them when they
/// come back.
const REMEMBERED: usize = 64;
/// How often [`run_lines`] writes everything again, at most.
const REPLAY_PAUSE: Duration = Duration::from_secs(1);

/// Whether a line read by [`run_lines`] is no command but what it wrote,
/// come back: the other end echoes (a serial port opened without turning
/// that off, which takes a moment even for those who do), or a terminal
/// answers the escape sequences the service manager wrote. An echo may
/// spell escapes out, as `^[` (a terminal's `echoctl`), which nobody types.
/// Answering it would only send more back.
#[must_use]
pub fn is_echo(line: &str, written: &std::collections::VecDeque<String>) -> bool {
    line.contains('\x1b') || line.contains("^[") || written.iter().any(|w| w == line)
}

/// The console for a line-based terminal, e.g. a serial port a program
/// reads: no screen to redraw, just what changes, a line each. Commands
/// come in as lines; an empty one (or `status`) has everything written
/// again, for whoever just opened the port: a USB gadget's drops what
/// nobody read. Runs until stdin, a terminal, hangs up (when the USB host
/// goes away, say), for the service manager to open it again; or forever,
/// if stdin is no terminal and ends.
pub async fn run_lines(state_dir: PathBuf, format: Lines) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let terminal = termios::isatty(std::io::stdin());
    // A program on the other end doesn't want its commands echoed back.
    let _quiet = (format == Lines::Json).then(Quiet::enter).transpose()?;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut out = Out::new(format);
    let mut watching: Option<Watching> = None;
    let mut seen: Option<Status> = None;
    let mut replayed: Option<std::time::Instant> = None;
    let mut open = true;
    loop {
        if watching.is_none() {
            watching = control::watch(&state_dir).await.ok();
        }
        tokio::select! {
            status = async {
                if let Some(watching) = &mut watching {
                    watching.next().await
                } else {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Err(anyhow::anyhow!("not connected"))
                }
            } => match status {
                Ok(status) => {
                    out.changes(seen.as_ref(), &status)?;
                    seen = Some(status);
                }
                Err(_) => watching = None,
            },
            line = input.next_line(), if open => match line {
                Ok(Some(line)) if is_echo(line.trim(), &out.written) => {}
                // CR LF is two empty lines to a terminal that turns CR into
                // LF: one is enough.
                Ok(Some(line)) if matches!(line.trim(), "" | "status") => {
                    if replayed.is_none_or(|at| at.elapsed() >= REPLAY_PAUSE) {
                        replayed = Some(std::time::Instant::now());
                        match &seen {
                            Some(status) => out.changes(None, status)?,
                            None => out.answer("status", "the agent isn't running (yet)")?,
                        }
                    }
                }
                Ok(Some(line)) => {
                    let line = line.trim();
                    let answer = command(&state_dir, line).await;
                    if !answer.is_empty() {
                        let word = line.split_whitespace().next().unwrap_or_default();
                        out.answer(word, &answer)?;
                    }
                }
                // A terminal that hung up stays so.
                Ok(None) | Err(_) if terminal => return Ok(()),
                // Without input, it still reports.
                Ok(None) | Err(_) => open = false,
            },
        }
    }
}

/// Writes [`run_lines`]' lines, and remembers the last of them.
struct Out {
    format: Lines,
    written: std::collections::VecDeque<String>,
}

impl Out {
    fn new(format: Lines) -> Self {
        Self {
            format,
            written: std::collections::VecDeque::with_capacity(REMEMBERED),
        }
    }

    fn line(&mut self, text: &str) -> Result<()> {
        if self.written.len() == REMEMBERED {
            self.written.pop_front();
        }
        self.written.push_back(text.trim().to_string());
        let mut out = std::io::stdout().lock();
        // A serial line wants CR LF.
        out.write_all(text.as_bytes())
            .and_then(|()| out.write_all(b"\r\n"))
            .and_then(|()| out.flush())
            .context("writing to the console")
    }

    fn json(&mut self, event: &str, mut fields: serde_json::Value) -> Result<()> {
        if let Some(object) = fields.as_object_mut() {
            object.insert("v".into(), 1.into());
            object.insert("event".into(), event.into());
        }
        self.line(&fields.to_string())
    }

    /// What changed from `before` to `now`.
    fn changes(&mut self, before: Option<&Status>, now: &Status) -> Result<()> {
        let code_changed = before.is_none_or(|b| b.code != now.code);
        let relay_changed = before.is_none_or(|b| b.relay != now.relay);
        match self.format {
            Lines::Json if code_changed || relay_changed => self.json(
                "status",
                serde_json::json!({
                    "code": now.code.as_ref().map(|c| &c.code),
                    "command": now.code.as_ref().map(|c| &c.command),
                    "relay": now.relay,
                }),
            )?,
            Lines::Json => {}
            Lines::Plain => {
                if code_changed {
                    match &now.code {
                        Some(shown) => {
                            self.line(&format!("code {}: {}", shown.code, shown.command))?;
                        }
                        None => self.line("code none")?,
                    }
                }
                if relay_changed && let Some(relay) = &now.relay {
                    self.line(relay)?;
                }
            }
        }
        // A restarted agent counts anew: then everything it has is new.
        let new = now
            .said
            .checked_sub(before.map_or(0, |b| b.said))
            .unwrap_or(now.said);
        let new = usize::try_from(new)
            .unwrap_or(usize::MAX)
            .min(now.messages.len());
        let first = now.messages.len().saturating_sub(new);
        for text in now.messages.get(first..).unwrap_or_default() {
            match self.format {
                Lines::Json => self.json("message", serde_json::json!({ "text": text }))?,
                Lines::Plain => self.line(text)?,
            }
        }
        Ok(())
    }

    /// The answer to `command` (its first word).
    fn answer(&mut self, command: &str, text: &str) -> Result<()> {
        match self.format {
            Lines::Json => self.json(
                "answer",
                serde_json::json!({ "command": command, "text": text }),
            ),
            Lines::Plain => self.line(&format!("> {command}: {text}")),
        }
    }
}

/// Echo off on the terminal until dropped, if stdin is one.
struct Quiet {
    saved: Option<Termios>,
}

impl Quiet {
    fn enter() -> Result<Self> {
        let stdin = std::io::stdin();
        let Ok(saved) = termios::tcgetattr(&stdin) else {
            return Ok(Self { saved: None });
        };
        let mut quiet = saved.clone();
        quiet.local_modes.remove(termios::LocalModes::ECHO);
        termios::tcsetattr(&stdin, OptionalActions::Now, &quiet)
            .context("turning the terminal's echo off")?;
        Ok(Self { saved: Some(saved) })
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            let _ = termios::tcsetattr(std::io::stdin(), OptionalActions::Now, saved);
        }
    }
}
