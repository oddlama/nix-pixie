//! Tests for `nix_pixie::console`: what the installer's tty1 shows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::console::{View, render, wrap};
use nix_pixie::screen::{Shown, Status};

/// The screen's lines without escape sequences.
fn lines(out: &str) -> Vec<String> {
    let mut plain = String::new();
    let mut escape = false;
    for c in out.chars() {
        if escape {
            escape = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            escape = true;
        } else {
            plain.push(c);
        }
    }
    plain.split("\r\n").map(str::to_string).collect()
}

fn status(messages: usize) -> Status {
    Status {
        code: Some(Shown {
            code: "482913-acid-acorn-acre".into(),
            command: "nix-pixie controller pair --code 482913-acid-acorn-acre".into(),
        }),
        relay: Some("Relay: https://relay.example.org/ is reachable.".into()),
        messages: (1..=messages).map(|n| format!("message {n}")).collect(),
        said: u64::try_from(messages).unwrap(),
    }
}

#[test]
fn shows_the_code_how_to_pair_the_relay_and_the_newest_messages() {
    let view = View {
        status: Some(status(100)),
        notice: None,
        input: "relay htt".into(),
    };
    let shown = lines(&render(&view, 80, 25));
    assert_eq!(shown.len(), 25, "{shown:#?}");
    assert!(shown.contains(&"Pairing code:  482913-acid-acorn-acre".to_string()));
    assert!(
        shown.contains(&"  nix-pixie controller pair --code 482913-acid-acorn-acre".to_string())
    );
    assert!(
        shown
            .iter()
            .any(|l| l.starts_with("Relay: https://relay.example.org/"))
    );
    assert!(
        shown
            .iter()
            .any(|l| l.starts_with("Alt+F2: a shell (tty2)"))
    );
    // As many of the newest messages as fit, oldest first.
    let messages: Vec<&String> = shown.iter().filter(|l| l.starts_with("message ")).collect();
    assert_eq!(messages.last().unwrap().as_str(), "message 100");
    assert!(messages.len() > 5 && !shown.contains(&"message 1".to_string()));
    assert_eq!(
        shown.last().unwrap(),
        "> relay htt",
        "the prompt comes last"
    );
}

#[test]
fn without_the_agent_it_says_so() {
    let shown = lines(&render(&View::default(), 80, 25));
    assert!(
        shown.iter().any(|l| l.starts_with("Waiting for the agent")),
        "{shown:#?}"
    );
    assert!(!shown.iter().any(|l| l.starts_with("Pairing code")));
}

#[test]
fn long_lines_wrap_at_spaces_and_long_words_are_cut() {
    assert_eq!(wrap("one two three", 7), ["one two", "three"]);
    assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    assert_eq!(wrap("", 10), [""]);
    assert_eq!(
        wrap("  one two three", 9),
        ["  one two", "  three"],
        "indented"
    );
    // Escape sequences take no room.
    assert_eq!(
        wrap("\x1b[1mbold\x1b[0m word", 9),
        ["\x1b[1mbold\x1b[0m word"]
    );
}
