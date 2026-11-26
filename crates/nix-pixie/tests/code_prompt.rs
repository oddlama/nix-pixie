//! Tests for `nix_pixie::ui::code`: completing and checking a code while it
//! is typed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::ui::code::{Nearby, complete, hint, invalid};
use nix_pixie_proto::code::completions;

/// What a Tab inserts, without targets nearby.
fn replacements(line: &str, pos: usize) -> (usize, Vec<String>) {
    let (start, found) = complete(line, pos, &Nearby::new());
    (
        start,
        found
            .into_iter()
            .map(|(replacement, _)| replacement)
            .collect(),
    )
}

#[test]
fn nameplates_complete_from_the_targets_nearby() {
    let nearby = Nearby::from([
        (482_913, "nixos".to_string()),
        (483_001, "web2".to_string()),
    ]);
    let (start, found) = complete("48", 2, &nearby);
    assert_eq!(start, 0);
    assert_eq!(
        found,
        [
            ("482913-".to_string(), "482913  nixos".to_string()),
            ("483001-".to_string(), "483001  web2".to_string())
        ]
    );
    assert_eq!(complete("4829", 4, &nearby).1.len(), 1);
    assert_eq!(
        complete("", 0, &nearby).1.len(),
        2,
        "all of them on an empty line"
    );
    assert_eq!(hint("4829", 4, &nearby).as_deref(), Some("13"));
    assert_eq!(hint("48", 2, &nearby), None, "two start like this");
    // Only the first token is a nameplate.
    assert_eq!(complete("482913-acid-4", 13, &nearby).1, []);
}

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
        replacements(line, line.len()),
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
    assert_eq!(replacements(line, line.len()), (12, vec!["acorn-".into()]));
    // The third word ends the code.
    let line = "482913-acid-acorn-zo";
    assert_eq!(
        replacements(line, line.len()),
        (18, vec!["zone".into(), "zoom".into()])
    );
    // Spaces separate words as well.
    let line = "482913 acid acorn acr";
    assert_eq!(replacements(line, line.len()), (18, vec!["acre".into()]));
}

#[test]
fn nameplates_and_unknown_words_complete_to_nothing() {
    assert_eq!(replacements("4829", 4), (0, Vec::new()));
    assert_eq!(replacements("482913-zz", 9), (7, Vec::new()));
    assert_eq!(replacements("482913-", 7), (7, Vec::new()));
}

#[test]
fn the_only_word_left_is_hinted() {
    assert_eq!(
        hint("482913-aco", 10, &Nearby::new()).as_deref(),
        Some("rn")
    );
    assert_eq!(
        hint("482913-ac", 9, &Nearby::new()),
        None,
        "several words start like this"
    );
    assert_eq!(
        hint("482913-acorn", 12, &Nearby::new()).as_deref(),
        Some("")
    );
    assert_eq!(
        hint("482913-aco", 9, &Nearby::new()),
        None,
        "not at the end of the line"
    );
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
