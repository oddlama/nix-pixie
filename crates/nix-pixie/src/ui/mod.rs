//! The operator's terminal: log lines, the steps of what is being done, and
//! what the operator is told and asked.
//!
//! **Live**, when stderr is a terminal: log lines scroll by above a region
//! at the bottom of the screen that is redrawn in place. It lists the steps,
//! known from the start, as `□` to do, a spinner while running and `▣` when
//! done, and below them what the operator is told or asked right now. An
//! answered question moves up into the scrollback together with its answer,
//! so the scrollback reads as a transcript.
//!
//! **Plain**, for pipes, files, the journal and `TERM=dumb`: everything is
//! printed line by line as it happens. Nothing is redrawn and nothing is
//! coloured.
//!
//! One global console owns stderr. Tracing's writer ([`log_writer`]), the
//! steps, questions and a thread that animates the spinner and reads
//! answers all go through its lock, so nothing lands in the middle of a
//! redraw. Programs that need the terminal for themselves (hooks, passphrase
//! prompts) get it with [`suspend`].

mod input;
pub mod render;

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::sync::{Condvar, Mutex, MutexGuard, Once, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

pub use input::{Input, Serviced};
pub use render::{Status, Step, Tone, duration, span};

use render::{FRAME, View, render};

static CONSOLE: OnceLock<Console> = OnceLock::new();
static TICKER: Once = Once::new();
/// How often the spinner thread looks at stdin and the clock.
const TICK: Duration = Duration::from_millis(40);
/// Terminals that know it show a redraw at once instead of line by line.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// How output is shown, see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    Plain,
}

/// How the console turned out, for the composition root to set up logging
/// to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    /// Whether to colour (stderr is a terminal and `NO_COLOR` is unset).
    pub colors: bool,
}

impl Settings {
    /// From the environment: live on a terminal that isn't `dumb`, colours
    /// there unless `NO_COLOR` says otherwise.
    pub fn detect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let live = std::io::stderr().is_terminal() && !term.is_empty() && term != "dumb";
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self {
            mode: if live { Mode::Live } else { Mode::Plain },
            colors: live && !no_color,
        }
    }
}

/// The console: how it shows things, and what it shows.
struct Console {
    settings: Settings,
    state: Mutex<State>,
    /// Woken when a line or the end of input arrives.
    answered: Notify,
    /// Wakes the thread that draws the spinner and reads stdin, which
    /// sleeps while it has nothing to do (always, in the agent).
    work: Condvar,
}

/// What the console shows, and where the region is on the screen.
#[derive(Debug, Default)]
struct State {
    steps: Vec<Step>,
    panel: Vec<String>,
    prompt: Option<String>,
    asking: bool,
    /// Rows of the region on the screen. The cursor is on the last one.
    drawn: usize,
    /// Nesting of [`suspend`]: while above 0, the terminal isn't ours.
    suspended: usize,
    /// After [`finish`]: the region is left as it is, output goes below.
    finished: bool,
    /// Something changed that the next tick should draw.
    dirty: bool,
    /// The spinner frame last drawn, counted from the console's start.
    frame: u128,
    input: Input,
    started: Option<Instant>,
}

/// Sets up the console from the environment. Call once, before anything is
/// printed; without it, the console is plain.
pub fn init() -> Settings {
    let console = CONSOLE.get_or_init(|| Console::new(Settings::detect()));
    start_ticker();
    console.settings
}

fn console() -> &'static Console {
    let console = CONSOLE.get_or_init(|| {
        Console::new(Settings {
            mode: Mode::Plain,
            colors: false,
        })
    });
    start_ticker();
    console
}

fn start_ticker() {
    TICKER.call_once(|| {
        // Without it there is no spinner and no answers; everything else
        // works, so there is no reason to fail.
        let _ = std::thread::Builder::new()
            .name("console".into())
            .spawn(|| {
                loop {
                    if let Some(console) = CONSOLE.get() {
                        console.idle();
                    }
                    std::thread::sleep(TICK);
                    if let Some(console) = CONSOLE.get() {
                        console.tick();
                    }
                }
            });
    });
}

/// Columns and rows of the terminal on stderr.
fn size() -> (usize, usize) {
    rustix::termios::tcgetwinsize(std::io::stderr())
        .map_or((0, 0), |w| (usize::from(w.ws_col), usize::from(w.ws_row)))
}

/// Writes `text` to stderr in one go. Errors (the terminal is gone) are
/// ignored: there is nowhere left to report them.
fn write(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

/// Moves from the region's last row to the start of its first.
fn to_top(drawn: usize, out: &mut String) {
    if drawn > 0 {
        out.push('\r');
    }
    if drawn > 1 {
        let _ = write!(out, "\x1b[{}A", drawn - 1);
    }
}

/// Writes `lines` from the cursor on, each clearing what was there before.
fn push_lines(out: &mut String, lines: &[String]) {
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
        out.push_str("\x1b[K");
    }
}

fn with_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

impl Console {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            state: Mutex::new(State {
                started: Some(Instant::now()),
                ..State::default()
            }),
            answered: Notify::new(),
            work: Condvar::new(),
        }
    }

    /// Whether the thread has anything to do: a region to animate or
    /// guard, or somebody waiting for a line.
    fn busy(&self, s: &State) -> bool {
        s.input.waiting > 0 || (self.drawing(s) && s.drawn > 0)
    }

    /// Blocks the thread until it has something to do.
    fn idle(&self) {
        let mut s = self.lock();
        while !self.busy(&s) {
            s = self.work.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self) -> bool {
        self.settings.mode == Mode::Live
    }

    /// Whether the region is ours to draw right now.
    fn drawing(&self, s: &State) -> bool {
        self.live() && s.suspended == 0 && !s.finished
    }

    fn region(&self, s: &State) -> Vec<String> {
        let (width, height) = size();
        render(&View {
            steps: &s.steps,
            panel: &s.panel,
            prompt: s.prompt.as_deref(),
            asking: s.asking,
            now: Instant::now(),
            width,
            height,
            colors: self.settings.colors,
        })
    }

    /// Draws the region over itself.
    fn redraw(&self, s: &mut State) {
        if !self.drawing(s) {
            return;
        }
        let lines = self.region(s);
        let mut out = String::from(SYNC_BEGIN);
        to_top(s.drawn, &mut out);
        push_lines(&mut out, &lines);
        out.push_str("\x1b[J");
        out.push_str(SYNC_END);
        write(&out);
        s.drawn = lines.len();
        s.dirty = false;
        self.work.notify_one();
    }

    /// Prints `text` into the scrollback: above the region, which moves down.
    fn print_above(&self, s: &mut State, text: &str) {
        if !self.drawing(s) {
            write(&with_newline(text));
            return;
        }
        let mut out = String::from(SYNC_BEGIN);
        to_top(s.drawn, &mut out);
        out.push_str("\x1b[J");
        out.push_str(&with_newline(text));
        let lines = self.region(s);
        push_lines(&mut out, &lines);
        out.push_str(SYNC_END);
        write(&out);
        s.drawn = lines.len();
        s.dirty = false;
        self.work.notify_one();
    }

    /// Clears the region from the screen, leaving the cursor where it began.
    fn clear(s: &mut State) {
        if s.drawn > 0 {
            let mut out = String::new();
            to_top(s.drawn, &mut out);
            out.push_str("\x1b[J");
            write(&out);
            s.drawn = 0;
        }
    }

    /// Reads answers and animates the spinner; runs every [`TICK`].
    fn tick(&self) {
        let mut s = self.lock();
        if s.suspended > 0 {
            return;
        }
        let wanted = s.input.waiting > 0;
        // A line typed while nobody asks was echoed into the region: take it
        // (a stray `y` mustn't answer the next question) and redraw.
        let guard = self.drawing(&s) && s.input.tty && s.drawn > 0 && input::foreground();
        if wanted || guard {
            let serviced = s.input.service(wanted);
            if self.live() && s.input.tty && s.drawn > 0 && serviced.newlines > 0 && input::echoes()
            {
                s.drawn += serviced.newlines;
                s.dirty = true;
            }
            if serviced.answered {
                self.answered.notify_waiters();
                // Whoever asked redraws with the answer.
                return;
            }
        }
        if !self.drawing(&s) {
            return;
        }
        let frame = s
            .started
            .map_or(0, |t| t.elapsed().as_millis() / FRAME.as_millis());
        let spinning = !s.asking
            && s.steps
                .iter()
                .any(|step| matches!(step.status, Status::Running { .. }));
        if s.dirty || (spinning && frame != s.frame) {
            s.frame = frame;
            self.redraw(&mut s);
        }
    }

    fn position(s: &State, key: &str) -> Option<usize> {
        s.steps.iter().position(|step| step.key == key)
    }

    /// Ends the step `key`, if it is running.
    fn end(&self, key: &str, end: impl FnOnce(Duration) -> Status, detail: Option<String>) {
        let mut s = self.lock();
        let Some(step) = Self::position(&s, key).and_then(|i| s.steps.get_mut(i)) else {
            return;
        };
        let Status::Running { since } = step.status else {
            return;
        };
        step.status = end(since.elapsed());
        if let Some(detail) = detail {
            step.detail = detail;
        }
        self.redraw(&mut s);
    }
}

/// Whether the console is live (see the module documentation).
pub fn live() -> bool {
    console().live()
}

/// Whether stdin is a terminal, i.e. somebody can answer and press Enter.
pub fn interactive() -> bool {
    console().lock().input.tty
}

/// Whether the console colours.
pub fn colors() -> bool {
    console().settings.colors
}

/// `text` in `tone`, if the console colours.
pub fn paint(text: &str, tone: Tone) -> String {
    render::paint(text, tone, colors())
}

/// Replaces the list of steps. Steps that were in the list before keep their
/// state, so the list can be refined as more becomes known.
pub fn steps(list: impl IntoIterator<Item = (String, String)>) {
    let console = console();
    let mut s = console.lock();
    let old = std::mem::take(&mut s.steps);
    s.steps = list
        .into_iter()
        .map(
            |(key, label)| match old.iter().find(|step| step.key == key) {
                Some(step) => Step {
                    label,
                    ..step.clone()
                },
                None => Step::todo(key, label),
            },
        )
        .collect();
    console.redraw(&mut s);
}

/// A running step. Dropping it before it is done marks it failed, so an
/// error that leaves a step shows where.
#[must_use = "dropping a running step marks it failed"]
#[derive(Debug)]
pub struct Running {
    key: String,
    ended: bool,
}

/// Starts the step `key`. A step that isn't in the list is added after the
/// last one that began.
pub fn start(key: &str) -> Running {
    start_with(key, "")
}

/// Starts the step `key`, saying what it does first. Plain output prints
/// the step's label, with the detail in parentheses.
pub fn start_with(key: &str, detail: impl Into<String>) -> Running {
    begin(key, &detail.into(), false)
}

/// Starts the step `key` like [`start_with`], and also prints its label
/// into the scrollback on a live console: for steps whose own output
/// follows there, like a hook's.
pub fn start_announced(key: &str, detail: impl Into<String>) -> Running {
    begin(key, &detail.into(), true)
}

fn begin(key: &str, detail: &str, announce: bool) -> Running {
    let console = console();
    let mut s = console.lock();
    let index = Console::position(&s, key).unwrap_or_else(|| {
        let after = s
            .steps
            .iter()
            .rposition(|step| step.status != Status::Todo)
            .map_or(0, |i| i + 1);
        s.steps.insert(after, Step::todo(key, key));
        after
    });
    if let Some(step) = s.steps.get_mut(index) {
        step.status = Status::Running {
            since: Instant::now(),
        };
        detail.clone_into(&mut step.detail);
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        let line = format!("▸ {}{detail}", step.label);
        if !console.live() {
            write(&with_newline(&line));
        } else if announce {
            let line = render::paint(&line, Tone::Dim, console.settings.colors);
            console.print_above(&mut s, &line);
        }
    }
    console.redraw(&mut s);
    Running {
        key: key.to_string(),
        ended: false,
    }
}

impl Running {
    /// What the step is doing right now. Cheap: drawn with the next frame,
    /// so it can be called for every line a program prints.
    pub fn detail(&self, text: impl Into<String>) {
        let console = console();
        let mut s = console.lock();
        if let Some(step) = Console::position(&s, &self.key).and_then(|i| s.steps.get_mut(i)) {
            step.detail = text.into();
            s.dirty = true;
        }
    }

    /// Marks the step done, with `detail` on how it ended. An empty one
    /// keeps the last detail, e.g. what Nix built in all.
    pub fn done(mut self, detail: impl Into<String>) {
        self.ended = true;
        let detail = Some(detail.into()).filter(|d| !d.is_empty());
        console().end(&self.key, |took| Status::Done { took }, detail);
    }

    /// Marks the step as not applying after all, saying why.
    pub fn skip(mut self, reason: impl Into<String>) {
        self.ended = true;
        console().end(&self.key, |_| Status::Skipped, Some(reason.into()));
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.ended {
            console().end(&self.key, |took| Status::Failed { took }, None);
        }
    }
}

/// What the running step is doing right now, for code that doesn't hold
/// its [`Running`]. Cheap, like [`Running::detail`].
pub fn progress(text: impl Into<String>) {
    let console = console();
    let mut s = console.lock();
    if let Some(step) = s
        .steps
        .iter_mut()
        .rev()
        .find(|step| matches!(step.status, Status::Running { .. }))
    {
        step.detail = text.into();
        s.dirty = true;
    }
}

/// Marks a step that hasn't started as not applying, saying why.
pub fn skip(key: &str, reason: impl Into<String>) {
    let console = console();
    let mut s = console.lock();
    if let Some(step) = Console::position(&s, key).and_then(|i| s.steps.get_mut(i))
        && step.status == Status::Todo
    {
        step.status = Status::Skipped;
        step.detail = reason.into();
        console.redraw(&mut s);
    }
}

/// Tells the operator `lines`, below the steps until replaced or
/// [`commit`]ed. Plain output prints them.
pub fn show(lines: Vec<String>) {
    let console = console();
    let mut s = console.lock();
    if console.drawing(&s) {
        s.panel = lines;
        console.redraw(&mut s);
    } else if !lines.is_empty() {
        // Set apart from the log lines before it.
        write(&format!("\n{}", with_newline(&lines.join("\n"))));
    }
}

/// Moves what [`show`] shows into the scrollback, for the record.
pub fn commit() {
    let console = console();
    let mut s = console.lock();
    let panel = std::mem::take(&mut s.panel);
    if !panel.is_empty() {
        console.print_above(&mut s, &panel.join("\n"));
    }
}

/// Prints `lines` into the scrollback.
pub fn note(lines: &[String]) {
    let console = console();
    let mut s = console.lock();
    console.print_above(&mut s, &lines.join("\n"));
}

/// Asks `question` below what is shown, and waits for a line. The question
/// is asked anew: what was typed ahead on a terminal is dropped. Afterwards
/// what was shown, the question and the answer go into the scrollback.
/// `None` once input has ended.
pub async fn ask(question: &str) -> Option<String> {
    let console = console();
    {
        let mut s = console.lock();
        s.input.drop_typeahead();
        if console.drawing(&s) {
            s.prompt = Some(question.to_string());
            s.asking = true;
            console.redraw(&mut s);
        } else {
            write(question);
        }
    }
    let answer = line().await;
    let mut s = console.lock();
    if s.prompt.take().is_some() {
        s.asking = false;
        let mut record = std::mem::take(&mut s.panel);
        record.push(format!(
            "{}{}",
            paint(question, Tone::Bold),
            answer.as_deref().unwrap_or_default()
        ));
        console.print_above(&mut s, &record.join("\n"));
    } else if !s.input.tty || answer.is_none() {
        // Nothing echoed the answer: show it, and end the question's line.
        write(&format!("{}\n", answer.as_deref().unwrap_or_default()));
    }
    answer
}

/// Asks a yes/no `question`. Anything but `y` or `yes` is no, so is the end
/// of input.
pub async fn confirm(question: &str) -> bool {
    yes_or_no(question, false).await
}

/// Asks a yes/no `question` whose empty answer is `default`. Anything but
/// `y`, `yes`, `n` and `no` is no, so is the end of input.
pub async fn yes_or_no(question: &str, default: bool) -> bool {
    ask(question)
        .await
        .is_some_and(|answer| match answer.trim().to_ascii_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
}

/// Runs `attempt` until it succeeds. On a terminal the operator is told why
/// it failed and asked whether to try `what` again, e.g. after typing a
/// wrong password into a hook, or fixing the configuration in another
/// terminal. Without one, the first failure is final.
pub async fn retrying<T>(
    what: &str,
    mut attempt: impl AsyncFnMut() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    loop {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if !interactive() || input_ended() {
            return Err(error);
        }
        show(vec![paint(&format!("{error:#}"), Tone::Bad)]);
        if !yes_or_no(&format!("{what} again? [Y/n] "), true).await {
            return Err(error);
        }
        tracing::info!(error = format!("{error:#}"), "trying again: {what}");
    }
}

/// Whether stdin ended for good: nothing will answer any more.
pub fn input_ended() -> bool {
    console().lock().input.ended()
}

/// Counts a waiter for [`line`] while it waits, also if it is cancelled.
struct Waiting;

impl Waiting {
    fn new() -> Self {
        let console = console();
        console.lock().input.waiting += 1;
        console.work.notify_one();
        Self
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut s = console().lock();
        s.input.waiting = s.input.waiting.saturating_sub(1);
    }
}

/// The next line from stdin; `None` for Ctrl-D on a terminal, or once
/// input ended ([`input_ended`]). Cancel-safe: a line is only taken when it
/// is returned.
pub async fn line() -> Option<String> {
    let console = console();
    let _waiting = Waiting::new();
    loop {
        let notified = console.answered.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let next = console.lock().input.next_line();
        match next {
            Some(Ok(line)) => return Some(line),
            Some(Err(())) => return None,
            None => notified.await,
        }
    }
}

/// While held, the terminal belongs to someone else (a hook, a passphrase
/// prompt): the region is cleared and neither drawn nor is stdin read.
#[must_use = "the terminal is given back when this is dropped"]
#[derive(Debug)]
pub struct Suspended(());

/// Hands the terminal over until the returned guard is dropped.
pub fn suspend() -> Suspended {
    let console = console();
    let mut s = console.lock();
    if s.suspended == 0 && console.drawing(&s) {
        Console::clear(&mut s);
    }
    s.suspended += 1;
    Suspended(())
}

impl Drop for Suspended {
    fn drop(&mut self) {
        let console = console();
        let mut s = console.lock();
        s.suspended = s.suspended.saturating_sub(1);
        console.redraw(&mut s);
    }
}

/// Leaves the region on the screen as the final word, below which follows
/// whatever comes next (an error, the result). Steps still running were cut
/// short and are marked failed.
pub fn finish() {
    let console = console();
    let mut s = console.lock();
    if s.finished {
        return;
    }
    for step in &mut s.steps {
        if let Status::Running { since } = step.status {
            step.status = Status::Failed {
                took: since.elapsed(),
            };
        }
    }
    s.asking = false;
    s.prompt = None;
    console.redraw(&mut s);
    if console.drawing(&s) && s.drawn > 0 {
        write("\n");
    }
    s.drawn = 0;
    s.finished = true;
}

/// Prints one line of the result to stdout. If stdout is the terminal too,
/// above the region, which would otherwise be drawn over it.
pub fn print_result(line: &str) {
    let console = console();
    let mut s = console.lock();
    let shared = console.drawing(&s) && s.drawn > 0 && std::io::stdout().is_terminal();
    if shared {
        Console::clear(&mut s);
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(with_newline(line).as_bytes());
    let _ = stdout.flush();
    if shared {
        console.redraw(&mut s);
    }
}

/// Where tracing writes: one log line per writer, printed above the region
/// when it is dropped. See [`log_writer`].
#[derive(Debug, Default)]
pub struct LogWriter {
    buf: Vec<u8>,
}

/// A writer for one log line, for `tracing_subscriber`'s `with_writer`.
pub fn log_writer() -> LogWriter {
    LogWriter::default()
}

impl std::io::Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let console = console();
        let mut s = console.lock();
        console.print_above(&mut s, &String::from_utf8_lossy(&self.buf));
    }
}
