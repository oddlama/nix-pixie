//! Tests for `nix_pixie::ui`: what the live region shows, and how answers
//! are split into lines.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::time::{Duration, Instant};

use nix_pixie::ui::render::{
    SPINNER, Status, Step, View, duration, paint, render, span, truncate, width, window, wrap,
};
use nix_pixie::ui::{Input, Serviced, Tone};

fn steps(statuses: &[Status]) -> Vec<Step> {
    statuses
        .iter()
        .enumerate()
        .map(|(i, status)| Step {
            status: *status,
            ..Step::todo(format!("s{i}"), format!("Step {i}"))
        })
        .collect()
}

fn view<'a>(steps: &'a [Step], panel: &'a [String], now: Instant) -> View<'a> {
    View {
        steps,
        panel,
        prompt: None,
        asking: false,
        now,
        width: 60,
        height: 24,
        colors: false,
    }
}

#[test]
fn widths_ignore_colours() {
    let painted = paint("▣ done", Tone::Good, true);
    assert_ne!(painted, "▣ done");
    assert_eq!(width(&painted), 6);
    assert_eq!(paint("x", Tone::Bad, false), "x");
    assert_eq!(width("wide: 漢字"), 10);
}

#[test]
fn truncates_to_columns_and_keeps_colours_balanced() {
    assert_eq!(truncate("short", 10), "short");
    assert_eq!(truncate("exactly ten", 11), "exactly ten");
    assert_eq!(truncate("a longer line", 6), "a lon…");
    let painted = format!("{} tail", paint("coloured text", Tone::Dim, true));
    let cut = truncate(&painted, 8);
    assert_eq!(width(&cut), 8);
    assert!(cut.starts_with("\x1b[2m"), "{cut:?}");
    assert!(cut.ends_with("…\x1b[0m"), "{cut:?}");
    // Wide characters never overflow.
    assert!(width(&truncate("漢字漢字", 4)) <= 4);
}

#[test]
fn wraps_at_spaces_with_the_indentation_kept() {
    assert_eq!(wrap("fits", 10), ["fits"]);
    assert_eq!(
        wrap("  disk      sda 76 GiB QEMU [HAS DATA: gpt]", 20),
        ["  disk      sda 76", "  GiB QEMU [HAS", "  DATA: gpt]"]
    );
    // An unbreakable word is cut.
    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJm8";
    let rows = wrap(key, 16);
    assert!(rows.iter().all(|row| width(row) <= 16), "{rows:?}");
    assert_eq!(rows.concat().replace(' ', ""), key.replace(' ', ""));
}

#[test]
fn durations_never_round_up() {
    assert_eq!(duration(Duration::from_millis(600)), "0.6s");
    assert_eq!(duration(Duration::from_millis(59_990)), "59.9s");
    assert_eq!(duration(Duration::from_secs(123)), "2m03s");
    assert_eq!(duration(Duration::from_mins(62)), "1h02m");
    assert_eq!(span(Duration::from_secs(30)), "30 s");
    assert_eq!(span(Duration::from_mins(10)), "10 min");
    assert_eq!(span(Duration::from_mins(90)), "1 h 30 min");
    assert_eq!(span(Duration::from_secs(90)), "90 s");
}

#[test]
fn shows_each_state_of_a_step() {
    let now = Instant::now();
    let list = steps(&[
        Status::Done {
            took: Duration::from_millis(1500),
        },
        Status::Running {
            since: now.checked_sub(Duration::from_millis(250)).unwrap(),
        },
        Status::Todo,
        Status::Skipped,
        Status::Failed {
            took: Duration::from_secs(2),
        },
    ]);
    let lines = render(&view(&list, &[], now));
    assert_eq!(lines.len(), 6, "{lines:#?}");
    assert!(lines[0].chars().all(|c| c == '─'));
    assert_eq!(lines[1], " ▣ Step 0    1.5s");
    // 250 ms into the step: the fourth frame.
    assert_eq!(lines[2], format!(" {} Step 1    0.2s", SPINNER[3]));
    assert_eq!(lines[3], " □ Step 2");
    assert_eq!(lines[4], " – Step 3");
    assert_eq!(lines[5], " ✗ Step 4    2.0s");
}

#[test]
fn a_step_waiting_for_the_operator_asks() {
    let now = Instant::now();
    let mut list = steps(&[Status::Running { since: now }]);
    list[0].detail = "what it does".into();
    let panel = vec!["Some context".to_string()];
    let mut v = view(&list, &panel, now);
    v.prompt = Some("Go on? [y/N] ");
    v.asking = true;
    let lines = render(&v);
    assert_eq!(
        lines,
        [
            "─".repeat(59),
            " ? Step 0    0.0s  what it does".to_string(),
            "─".repeat(59),
            "Some context".to_string(),
            // The cursor waits after the space.
            "Go on? [y/N] ".to_string(),
        ]
    );
}

#[test]
fn every_row_fits_the_terminal() {
    let now = Instant::now();
    let mut list = steps(&[Status::Running { since: now }]);
    list[0].detail = "x".repeat(200);
    let panel = vec!["y ".repeat(100)];
    let lines = render(&view(&list, &panel, now));
    assert!(lines.iter().all(|l| width(l) < 60), "{lines:#?}");
    // The panel is wrapped, not cut.
    let said: String = lines.iter().skip(3).cloned().collect();
    assert_eq!(said.matches('y').count(), 100);
}

#[test]
fn long_lists_show_the_current_step() {
    let now = Instant::now();
    let mut statuses = vec![
        Status::Done {
            took: Duration::ZERO
        };
        12
    ];
    statuses.push(Status::Running { since: now });
    statuses.extend([Status::Todo; 10]);
    let list = steps(&statuses);
    let (start, end) = window(&list, 9);
    assert_eq!(end - start, 9);
    assert!((start + 1..end - 1).contains(&12), "{start}..{end}");

    let mut v = view(&list, &[], now);
    v.height = 12;
    let lines = render(&v);
    assert!(lines.len() < 12, "{lines:#?}");
    assert!(lines[1].contains("earlier"), "{lines:#?}");
    assert!(lines.last().unwrap().contains("more"), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("Step 12")), "{lines:#?}");
}

#[test]
fn a_question_fits_even_without_room_for_the_steps() {
    let now = Instant::now();
    let list = steps(&[Status::Running { since: now }; 5]);
    let panel: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
    let mut v = view(&list, &panel, now);
    v.height = 6;
    v.prompt = Some("Sure? ");
    let lines = render(&v);
    assert_eq!(lines.len(), 5, "{lines:#?}");
    assert_eq!(lines.last().unwrap(), "Sure? ");
    assert!(!lines.iter().any(|l| l.contains("Step")), "{lines:#?}");
}

#[test]
fn nothing_to_show_draws_nothing() {
    assert_eq!(
        render(&view(&[], &[], Instant::now())),
        Vec::<String>::new()
    );
}

#[test]
fn input_keeps_lines_only_when_asked_for() {
    let mut input = Input::new(true);
    assert_eq!(
        input.take(b"typed ahead\n", false),
        Serviced {
            newlines: 1,
            answered: false
        }
    );
    assert_eq!(input.next_line(), None);
    assert_eq!(
        input.take(b"y\r\nn", true),
        Serviced {
            newlines: 1,
            answered: true
        }
    );
    assert_eq!(input.next_line(), Some(Ok("y".into())));
    assert_eq!(input.next_line(), None);
    input.take(b"o\n", true);
    assert_eq!(input.next_line(), Some(Ok("no".into())));
}

#[test]
fn ctrl_d_on_a_terminal_ends_an_answer_not_the_input() {
    let mut input = Input::new(true);
    // Nobody asked: nothing changes.
    assert!(!input.nothing(false, false).answered);
    assert!(input.next_line().is_none());
    assert!(input.nothing(true, false).answered);
    assert_eq!(input.next_line(), Some(Err(())));
    assert_eq!(input.next_line(), None);
    assert!(!input.ended());
    input.take(b"y\n", true);
    assert_eq!(input.next_line(), Some(Ok("y".into())));
    // Hung up: over for good.
    input.nothing(false, true);
    assert!(input.ended());
    assert_eq!(input.next_line(), Some(Err(())));
}

#[test]
fn the_end_of_a_pipe_ends_the_input() {
    let mut input = Input::new(false);
    input.take(b"y\nlast", true);
    assert!(input.nothing(true, false).answered);
    assert!(input.ended());
    assert_eq!(input.next_line(), Some(Ok("y".into())));
    assert_eq!(input.next_line(), Some(Ok("last".into())));
    assert_eq!(input.next_line(), Some(Err(())));
}
