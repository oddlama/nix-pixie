//! Tests for `nix_pixie_proto::code`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::code::*;

#[test]
fn wordlist_shape() {
    let list = wordlist();
    assert_eq!(list.len(), 1295);
    assert!(list.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    assert!(
        list.iter()
            .all(|w| (3..=5).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()))
    );
}

/// Typing any word on a QWERTZ or AZERTY keyboard (with a US console
/// keymap) must normalise back to exactly that word.
#[test]
fn layouts_are_unambiguous() {
    for word in wordlist() {
        for typed in layout_variants(word) {
            assert_eq!(normalise_word(&typed), Ok(*word), "{word} typed as {typed}");
        }
    }
}

#[test]
fn round_trip() {
    for nameplate in [None, Some(1), Some(42), Some(MAX_NAMEPLATE)] {
        let code = Code::generate(nameplate).unwrap();
        assert_eq!(Code::parse(&code.to_string()), Ok(code));
    }
}

#[test]
fn parse_is_forgiving_about_separators_and_case() {
    let expected = Code::parse("42-acid-acorn-acre").unwrap();
    assert_eq!(expected.to_string(), "42-acid-acorn-acre");
    for typed in [
        "42 acid acorn acre",
        " 42--ACID acorn,acre ",
        "42/acid.acorn_acre",
    ] {
        assert_eq!(Code::parse(typed), Ok(expected), "{typed}");
    }
}

#[test]
fn parse_undoes_keyboard_layouts() {
    // "lazy" typed on QWERTZ with a US keymap comes out as "layz".
    let code = Code::parse("acid-lazy-zesty").unwrap();
    assert_eq!(Code::parse("acid-layz-yestz"), Ok(code));
    // AZERTY: a/q and w/z swapped, m on the `;` key.
    let code = Code::parse("swear-mumbo-aqua").unwrap();
    assert_eq!(Code::parse("szeqr-;u;bo-qauq"), Ok(code));
}

#[test]
fn parse_errors() {
    assert_eq!(Code::parse("acid acorn"), Err(CodeError::WordCount(2)));
    assert_eq!(
        Code::parse("0-acid-acorn-acre"),
        Err(CodeError::Nameplate("0".into()))
    );
    assert_eq!(
        Code::parse("100-acid-acorn-acre"),
        Err(CodeError::Nameplate("100".into()))
    );
    assert_eq!(
        Code::parse("acid-acorn-xyzzy"),
        Err(CodeError::UnknownWord("xyzzy".into()))
    );
}

#[test]
fn free_nameplate_avoids_taken_ones() {
    let taken: Vec<u8> = (1..MAX_NAMEPLATE).collect();
    assert_eq!(Code::free_nameplate(&taken), Some(MAX_NAMEPLATE));
    let all: Vec<u8> = (1..=MAX_NAMEPLATE).collect();
    assert_eq!(Code::free_nameplate(&all), None);
}
