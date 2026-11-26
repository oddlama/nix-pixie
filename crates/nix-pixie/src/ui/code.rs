//! Typing a pairing code on the controller's terminal. Words complete with
//! Tab and are checked as they are typed, and an invalid code is refused on
//! Enter without losing what was typed. Unlike `--code`, the code never
//! shows up in the shell's history or the process list.
//!
//! Completion only uses the public wordlist, so it reveals nothing about
//! the code. Nothing typed is kept: there is no history.

use std::borrow::Cow;

use anyhow::{Context as _, Result, bail};
use nix_pixie_proto::code::{Code, WORDS, completions};
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Behavior, CompletionType, Config, Context, Editor, error::ReadlineError};

/// What the code needs besides its words.
pub const NAMEPLATE_NEEDED: &str =
    "target codes start with the number shown on the target, e.g. 482913-acid-acorn-acre";

/// What a token of a typed code is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Nameplate,
    /// The start (or all) of at least one word.
    Word,
    /// No word starts like this.
    Unknown,
}

fn is_separator(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || c == ';')
}

fn kind(token: &str) -> Token {
    if token.bytes().all(|b| b.is_ascii_digit()) {
        Token::Nameplate
    } else if completions(token).is_empty() {
        Token::Unknown
    } else {
        Token::Word
    }
}

/// The token the cursor at `pos` is in, and where it starts.
fn current(line: &str, pos: usize) -> (usize, &str) {
    let before = line.get(..pos).unwrap_or(line);
    let start = before.rfind(is_separator).map_or(0, |i| i + 1);
    (start, before.get(start..).unwrap_or_default())
}

/// The words a Tab at `pos` may complete to, and where they go. Each ends
/// with a `-` unless it is the code's last word.
#[must_use]
pub fn complete(line: &str, pos: usize) -> (usize, Vec<String>) {
    let (start, token) = current(line, pos);
    if token.is_empty() || kind(token) == Token::Nameplate {
        return (start, Vec::new());
    }
    let words_before = line
        .get(..start)
        .unwrap_or_default()
        .split(is_separator)
        .filter(|t| !t.is_empty() && kind(t) != Token::Nameplate)
        .count();
    let dash = if words_before + 1 < WORDS { "-" } else { "" };
    let words = completions(token)
        .iter()
        .map(|word| format!("{word}{dash}"))
        .collect();
    (start, words)
}

/// The rest of the only word the token at the end of `line` can become.
#[must_use]
pub fn hint(line: &str, pos: usize) -> Option<String> {
    if pos != line.len() {
        return None;
    }
    let (_, token) = current(line, pos);
    match completions(token) {
        [word] if !token.is_empty() && kind(token) == Token::Word => {
            word.get(token.len()..).map(str::to_string)
        }
        _ => None,
    }
}

/// Why `line` isn't a target's code, if it isn't.
#[must_use]
pub fn invalid(line: &str) -> Option<String> {
    match Code::parse(line) {
        Ok(code) if code.nameplate().is_some() => None,
        Ok(_) => Some(NAMEPLATE_NEEDED.into()),
        Err(e) => Some(e.to_string()),
    }
}

/// The editor's helper for a code.
struct CodeHelper {
    colors: bool,
}

impl Completer for CodeHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, words) = complete(line, pos);
        let pairs = words
            .into_iter()
            .map(|word| Pair {
                display: word.trim_end_matches('-').to_string(),
                replacement: word,
            })
            .collect();
        Ok((start, pairs))
    }
}

impl Hinter for CodeHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _: &Context<'_>) -> Option<String> {
        hint(line, pos)
    }
}

impl Highlighter for CodeHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.colors {
            return Cow::Borrowed(line);
        }
        // Tokens no word starts with, in red: a typo shows at once.
        let mut out = String::with_capacity(line.len());
        let mut token = String::new();
        let flush = |token: &mut String, out: &mut String| {
            if !token.is_empty() && kind(token) == Token::Unknown {
                out.push_str("\x1b[31m");
                out.push_str(token);
                out.push_str("\x1b[0m");
            } else {
                out.push_str(token);
            }
            token.clear();
        };
        for c in line.chars() {
            if is_separator(c) {
                flush(&mut token, &mut out);
                out.push(c);
            } else {
                token.push(c);
            }
        }
        flush(&mut token, &mut out);
        Cow::Owned(out)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if self.colors {
            Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
        } else {
            Cow::Borrowed(hint)
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.colors
    }
}

impl Validator for CodeHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        Ok(match invalid(ctx.input()) {
            Some(reason) => ValidationResult::Invalid(Some(format!("  ({reason})"))),
            None => ValidationResult::Valid(None),
        })
    }
}

impl rustyline::Helper for CodeHelper {}

/// Asks for the code shown on the target, on the terminal.
///
/// # Errors
///
/// Without a terminal, or if the operator gives up (Ctrl-C, Ctrl-D).
pub async fn ask_target_code() -> Result<Code> {
    if !super::interactive() {
        bail!("--code is needed when not running on a terminal");
    }
    let colors = super::colors();
    // The editor reads the terminal itself.
    let _terminal = super::suspend();
    let line = tokio::task::spawn_blocking(move || -> Result<String> {
        let config = Config::builder()
            .auto_add_history(false)
            .completion_type(CompletionType::List)
            // The terminal itself: stdout may be a file for the result.
            .behavior(Behavior::PreferTerm)
            .build();
        let mut editor = Editor::<CodeHelper, DefaultHistory>::with_config(config)
            .context("starting the code prompt")?;
        editor.set_helper(Some(CodeHelper { colors }));
        match editor.readline("Code shown on the target: ") {
            Ok(line) => Ok(line),
            Err(ReadlineError::Interrupted) => bail!("interrupted"),
            Err(ReadlineError::Eof) => bail!("no code given"),
            Err(e) => Err(e).context("reading the code"),
        }
    })
    .await
    .context("the code prompt stopped")??;
    Ok(Code::parse(&line)?)
}
