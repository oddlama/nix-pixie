//! Tests for `nix_pixie::serial`: reading the target's answer off its
//! status terminal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::os::unix::fs::MetadataExt;

use nix_pixie::serial::{answer, openers};

#[test]
fn answers_to_code_come_plain_or_as_json() {
    assert_eq!(
        answer("> code: waiting for the controller with that code").as_deref(),
        Some("waiting for the controller with that code")
    );
    assert_eq!(
        answer(r#"{"v":1,"event":"answer","command":"code","text":"already waiting"}"#).as_deref(),
        Some("already waiting")
    );
}

#[test]
fn everything_else_is_no_answer_to_code() {
    // Answers to other commands, such as garbage echoed back at the target.
    assert_eq!(answer("> relay: now using n0's public relays"), None);
    assert_eq!(
        answer("> \x1b[!pcode: no such command (help lists them)"),
        None
    );
    assert_eq!(
        answer(r#"{"v":1,"event":"answer","command":"relay","text":"now using n0"}"#),
        None
    );
    // Status, messages, and our own command echoed back.
    assert_eq!(answer("code none"), None);
    assert_eq!(answer("code ash-bat-cow"), None);
    assert_eq!(answer("Relay: none"), None);
    assert_eq!(
        answer(r#"{"v":1,"event":"message","text":"> code: hi"}"#),
        None
    );
    assert_eq!(answer(r#"{"v":1,"event":"status","code":null}"#), None);
}

#[test]
fn other_processes_that_have_the_port_open_are_found() {
    let rdev = std::fs::metadata("/dev/null").unwrap().rdev();
    let mut holder = std::process::Command::new("sleep")
        .arg("30")
        .stdin(std::fs::File::open("/dev/null").unwrap())
        .spawn()
        .unwrap();
    let found = openers(rdev);
    holder.kill().unwrap();
    holder.wait().unwrap();
    let holder = format!("{} (sleep)", holder.id());
    assert!(found.contains(&holder), "{holder} not in {found:?}");
}
