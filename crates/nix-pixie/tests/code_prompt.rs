//! Tests for `nix_pixie::ui::code`: completing and checking a code while it
//! is typed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::ui::code::{complete, hint, invalid};
use nix_pixie_proto::code::completions;

#[test]
fn words_complete_from_the_list() {
    assert_eq!(completions("ac"), ["acid", "acorn", "acre", "acts"]);
    assert_eq!(completions("ACO"), ["acorn"], "any case");
    assert_eq!(completions("zoom"), ["zoom"]);
    assert_eq!(completions("zz"), [] as [&str; 0]);
    assert_eq!(
        completions("").len(),
        nix_pixie_proto::code::wordlist().len()
    );
}

#[test]
fn tab_completes_the_word_at_the_cursor_with_a_dash_until_the_last() {
    let line = "482913-ac";
    assert_eq!(
        complete(line, line.len()),
        (
            7,
            vec![
                "acid-".into(),
                "acorn-".into(),
                "acre-".into(),
                "acts-".into()
            ]
        )
    );
    let line = "482913-acid-aco";
    assert_eq!(complete(line, line.len()), (12, vec!["acorn-".into()]));
    // The third word ends the code.
    let line = "482913-acid-acorn-zo";
    assert_eq!(
        complete(line, line.len()),
        (18, vec!["zone".into(), "zoom".into()])
    );
    // Spaces separate words as well.
    let line = "482913 acid acorn acr";
    assert_eq!(complete(line, line.len()), (18, vec!["acre".into()]));
}

#[test]
fn nameplates_and_unknown_words_complete_to_nothing() {
    assert_eq!(complete("4829", 4), (0, Vec::new()));
    assert_eq!(complete("482913-zz", 9), (7, Vec::new()));
    assert_eq!(complete("482913-", 7), (7, Vec::new()));
}

#[test]
fn the_only_word_left_is_hinted() {
    assert_eq!(hint("482913-aco", 10).as_deref(), Some("rn"));
    assert_eq!(hint("482913-ac", 9), None, "several words start like this");
    assert_eq!(hint("482913-acorn", 12).as_deref(), Some(""));
    assert_eq!(hint("482913-aco", 9), None, "not at the end of the line");
}

#[test]
fn enter_refuses_what_is_not_a_target_code() {
    assert_eq!(invalid("482913-acid-acorn-acre"), None);
    assert!(
        invalid("482913-acid-acorn")
            .unwrap()
            .contains("expected 3 words")
    );
    assert!(invalid("482913-acid-acorn-zzz").unwrap().contains("zzz"));
    assert!(
        invalid("acid-acorn-acre")
            .unwrap()
            .contains("start with the number"),
        "a controller's code"
    );
    assert!(invalid("").is_some());
}
