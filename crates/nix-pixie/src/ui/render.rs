//! Turning the console's state into lines. Pure, so it can be tested
//! without a terminal.

use std::fmt::Write;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

/// How a piece of text is shown. Colour marks state; nothing is coloured
/// for decoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Secondary: details, rules, durations, steps still to come.
    Dim,
    /// Wants the operator's attention: a question.
    Bold,
    /// Finished successfully.
    Good,
    /// In progress.
    Active,
    /// Waits for the operator, or deserves a second look (data on a disk).
    Warn,
    /// Failed.
    Bad,
}

impl Tone {
    fn sgr(self) -> &'static str {
        match self {
            Self::Dim => "2",
            Self::Bold => "1",
            Self::Good => "32",
            Self::Active => "36",
            Self::Warn => "33",
            Self::Bad => "31",
        }
    }
}

const RESET: &str = "\x1b[0m";

/// `text` in `tone`, or unchanged without colours.
pub fn paint(text: &str, tone: Tone, colors: bool) -> String {
    if colors && !text.is_empty() {
        format!("\x1b[{}m{text}{RESET}", tone.sgr())
    } else {
        text.to_string()
    }
}

/// Consumes an escape sequence after its ESC: CSI (`ESC [ … final`), or a
/// lone ESC followed by one character.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: Option<&mut String>) {
    let mut sequence = String::from('\x1b');
    if chars.peek() == Some(&'[') {
        sequence.extend(chars.next());
        for c in chars.by_ref() {
            sequence.push(c);
            if ('\x40'..='\x7e').contains(&c) {
                break;
            }
        }
    } else {
        sequence.extend(chars.next());
    }
    if let Some(out) = out {
        out.push_str(&sequence);
    }
}

/// The columns `text` takes on a terminal, not counting escape sequences.
pub fn width(text: &str) -> usize {
    let mut chars = text.chars().peekable();
    let mut columns = 0;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars, None);
        } else {
            columns += c.width().unwrap_or(0);
        }
    }
    columns
}

/// `text` cut to at most `max` columns, ending in `…` if it was cut. Escape
/// sequences are kept, and reset at the end of a cut.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let limit = max.saturating_sub(1);
    let mut out = String::new();
    let mut columns = 0;
    let mut styled = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            styled = true;
            skip_escape(&mut chars, Some(&mut out));
            continue;
        }
        let w = c.width().unwrap_or(0);
        if columns + w > limit {
            break;
        }
        out.push(c);
        columns += w;
    }
    if max > 0 {
        out.push('…');
    }
    if styled {
        out.push_str(RESET);
    }
    out
}

/// `text` split into rows of at most `max` columns, at spaces where it can
/// be. Rows after the first keep the line's indentation. Escape sequences
/// stay where they are, so a colour carries over into the next row.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    if width(text) <= max {
        return vec![text.to_string()];
    }
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let indent = if indent.len() * 2 < max {
        indent
    } else {
        String::new()
    };
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut columns = 0;
    for (i, word) in text.split(' ').enumerate() {
        let w = width(word);
        let space = usize::from(i > 0);
        if columns > indent.len() && columns + space + w > max {
            rows.push(std::mem::take(&mut row));
            row.clone_from(&indent);
            columns = indent.len();
        } else if i > 0 {
            row.push(' ');
            columns += 1;
        }
        if columns + w <= max {
            row.push_str(word);
            columns += w;
            continue;
        }
        // Longer than a row: cut it wherever.
        let mut chars = word.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                skip_escape(&mut chars, Some(&mut row));
                continue;
            }
            let cw = c.width().unwrap_or(0);
            if columns + cw > max {
                rows.push(std::mem::take(&mut row));
                row.clone_from(&indent);
                columns = indent.len();
            }
            row.push(c);
            columns += cw;
        }
    }
    rows.push(row);
    rows
}

/// A duration for people: `0.6s`, `12.7s`, `2m03s`, `1h02m`. Never rounds
/// up, so a running clock doesn't jump.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}.{}s", d.subsec_millis() / 100)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    }
}

/// A wait for people: `30 s`, `10 min`, `1 h 30 min`.
pub fn span(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs % 3600 / 60, secs % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, 0) => format!("{m} min"),
        (h, 0, 0) => format!("{h} h"),
        (h, m, 0) => format!("{h} h {m} min"),
        _ => format!("{secs} s"),
    }
}

/// One step of what the controller does, as shown in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Identifies the step while the list is refined.
    pub key: String,
    /// What the step does, in the imperative: "Build nixosConfigurations.web1".
    pub label: String,
    pub status: Status,
    /// What it is doing right now, or how it ended.
    pub detail: String,
}

impl Step {
    /// A step still to come.
    pub fn todo(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            status: Status::Todo,
            detail: String::new(),
        }
    }
}

/// Where a step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Todo,
    Running {
        since: Instant,
    },
    Done {
        took: Duration,
    },
    Failed {
        took: Duration,
    },
    /// Turned out not to apply, e.g. there is no SSH server to check.
    Skipped,
}

/// Braille spinner frames, one every [`FRAME`].
pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// How long a spinner frame shows.
pub const FRAME: Duration = Duration::from_millis(80);
/// Labels longer than this don't widen the column for all others.
const MAX_LABEL_COLUMN: usize = 48;
/// The narrowest list worth showing: a step before, the current one, and
/// one after.
const MIN_STEP_ROWS: usize = 3;

/// Everything the live region shows.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    pub steps: &'a [Step],
    /// What the operator is told right now, below the steps.
    pub panel: &'a [String],
    /// The question being asked: the last line, where the cursor waits.
    pub prompt: Option<&'a str>,
    /// Whether the running step waits for the operator.
    pub asking: bool,
    pub now: Instant,
    /// Of the terminal, in columns and rows.
    pub width: usize,
    pub height: usize,
    pub colors: bool,
}

/// The live region's rows, top to bottom: a rule, the steps, and if
/// anything is said or asked, another rule and that. Every row fits the
/// terminal's width (steps are cut, what is said is wrapped), and all of
/// them its height (minus a row, so the top one never scrolls away), so the
/// region can be redrawn in place.
pub fn render(view: &View<'_>) -> Vec<String> {
    let width = if view.width == 0 { 80 } else { view.width };
    let height = if view.height == 0 { 24 } else { view.height };
    let columns = width.saturating_sub(1);
    let fit = |line: &str| truncate(line, columns);
    let rule = paint(&"─".repeat(columns), Tone::Dim, view.colors);
    let available = height.saturating_sub(1).max(1);

    let mut said: Vec<String> = view.panel.iter().flat_map(|l| wrap(l, columns)).collect();
    if let Some(prompt) = view.prompt {
        said.extend(wrap(&paint(prompt, Tone::Bold, view.colors), columns));
    }
    if said.len() + 1 > available {
        // Only what is said fits, and not all of it: keep its end, where a
        // question is.
        let keep = available.saturating_sub(1);
        let mut lines = vec![paint("…", Tone::Dim, view.colors)];
        lines.extend(said.split_off(said.len().saturating_sub(keep)));
        return lines;
    }
    let said_rows = if said.is_empty() { 0 } else { said.len() + 1 };
    let step_rows = available.saturating_sub(said_rows + 1);

    let mut lines = Vec::new();
    if !view.steps.is_empty() && step_rows >= MIN_STEP_ROWS {
        lines.push(rule.clone());
        lines.extend(step_lines(view, step_rows).iter().map(|l| fit(l)));
    }
    if !said.is_empty() {
        lines.push(rule);
        lines.extend(said);
    }
    lines
}

/// The steps, in at most `rows` lines: around the current step if they
/// don't all fit.
fn step_lines(view: &View<'_>, rows: usize) -> Vec<String> {
    let steps = view.steps;
    let (start, end) = window(steps, rows);
    let shown = steps.get(start..end).unwrap_or_default();
    let column = shown
        .iter()
        .map(|s| width(&s.label))
        .filter(|w| *w <= MAX_LABEL_COLUMN)
        .max()
        .unwrap_or(0);
    let elided = |text: String| format!("   {}", paint(&text, Tone::Dim, view.colors));
    let mut lines = Vec::new();
    for (i, step) in shown.iter().enumerate() {
        if i == 0 && start > 0 {
            lines.push(elided(format!("… {} earlier", start + 1)));
        } else if i + 1 == shown.len() && end < steps.len() {
            lines.push(elided(format!("… {} more", steps.len() - end + 1)));
        } else {
            lines.push(step_line(view, step, column));
        }
    }
    lines
}

/// The range of steps to show in `rows` lines, with the current step in its
/// upper third. The first and last line of the range make way for "… more"
/// where steps are left out.
pub fn window(steps: &[Step], rows: usize) -> (usize, usize) {
    if steps.len() <= rows {
        return (0, steps.len());
    }
    let current = steps
        .iter()
        .position(|s| matches!(s.status, Status::Running { .. }))
        .or_else(|| steps.iter().position(|s| s.status == Status::Todo))
        .unwrap_or(steps.len().saturating_sub(1));
    let end = (current.saturating_sub(rows / 3) + rows).min(steps.len());
    (end.saturating_sub(rows), end)
}

fn step_line(view: &View<'_>, step: &Step, column: usize) -> String {
    let colors = view.colors;
    // Plain labels for what is or was going on, dim ones for the rest.
    let (icon, dim, time, detail_tone) = match step.status {
        Status::Todo => (paint("□", Tone::Dim, colors), true, None, Tone::Dim),
        Status::Running { since } => {
            let icon = if view.asking {
                paint("?", Tone::Warn, colors)
            } else {
                let frame =
                    view.now.saturating_duration_since(since).as_millis() / FRAME.as_millis();
                let frame = usize::try_from(frame % 10).unwrap_or(0);
                paint(
                    &SPINNER.get(frame).copied().unwrap_or('⠋').to_string(),
                    Tone::Active,
                    colors,
                )
            };
            let took = view.now.saturating_duration_since(since);
            (icon, false, Some(took), Tone::Dim)
        }
        Status::Done { took } => (paint("▣", Tone::Good, colors), false, Some(took), Tone::Dim),
        Status::Failed { took } => (paint("✗", Tone::Bad, colors), false, Some(took), Tone::Bad),
        Status::Skipped => (paint("–", Tone::Dim, colors), true, None, Tone::Dim),
    };
    let text = if dim {
        paint(&step.label, Tone::Dim, colors)
    } else {
        step.label.clone()
    };
    let mut line = format!(" {icon} {text}");
    let pad = column.saturating_sub(width(&step.label));
    line.extend(std::iter::repeat_n(' ', pad));
    let time = time.map_or_default(duration);
    let _ = write!(
        line,
        "  {}",
        paint(&format!("{time:>6}"), Tone::Dim, colors)
    );
    if !step.detail.is_empty() {
        let _ = write!(line, "  {}", paint(&step.detail, detail_tone, colors));
    }
    line.trim_end().to_string()
}
