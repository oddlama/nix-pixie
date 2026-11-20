//! Pairing codes: an optional public nameplate plus three secret words.
//!
//! * Target-shown codes (the default pairing mode) look like `42-acid-acorn-acre`.
//!   The nameplate `42` is public: the target advertises it over DNS-SD so the
//!   controller knows which machine to dial. Only the words are secret.
//! * Controller-shown codes have no nameplate: `acid-acorn-acre`.
//!
//! Three words from the 1295-word list give about 31 bits. That is only safe
//! because the code is used as a SPAKE2 password: every guess costs an online
//! attempt, and codes rotate after a few failures.
//!
//! Codes are typed on the installer's Linux console, which uses a US keymap.
//! Someone with a QWERTZ or AZERTY keyboard who types the letters printed on
//! their keys produces swapped letters (`y`/`z`; `a`/`q`, `w`/`z`, `m`/`;`).
//! [`Code::parse`] undoes that per word. The wordlist is checked (see tests) so
//! that no word can turn into a different word that way.

use std::fmt;
use std::sync::OnceLock;

use rand::seq::IndexedRandom;

/// Number of secret words in a code.
pub const WORDS: usize = 3;
/// Nameplates are chosen from `1..=MAX_NAMEPLATE`.
pub const MAX_NAMEPLATE: u8 = 99;

const WORDLIST_SOURCE: &str = include_str!("wordlist.txt");

/// The pairing wordlist, in file order.
#[must_use]
pub fn wordlist() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        WORDLIST_SOURCE
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .collect()
    })
}

/// The list's own spelling of `word`, if it is on the list.
fn lookup(word: &str) -> Option<&'static str> {
    // The list is sorted; the tests check that.
    let list = wordlist();
    list.binary_search(&word)
        .ok()
        .and_then(|i| list.get(i))
        .copied()
}

/// A parsed or generated pairing code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code {
    nameplate: Option<u8>,
    /// Words from [`wordlist`], in order.
    words: [&'static str; WORDS],
}

/// Why a typed code was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodeError {
    #[error("expected {WORDS} words, got {0}")]
    WordCount(usize),
    #[error("`{0}` is not a valid nameplate (1-{MAX_NAMEPLATE})")]
    Nameplate(String),
    #[error("`{0}` is not a code word")]
    UnknownWord(String),
    #[error("`{0}` matches several code words ({1}); please retype it")]
    AmbiguousWord(String, String),
    #[error("the built-in wordlist is empty")]
    EmptyWordlist,
}

impl Code {
    /// Generates a code with fresh random words.
    ///
    /// # Errors
    ///
    /// Only if the built-in wordlist were empty (the tests check it isn't).
    /// Returned rather than papered over: a fallback would be a code without
    /// any secret.
    pub fn generate(nameplate: Option<u8>) -> Result<Self, CodeError> {
        let list = wordlist();
        let mut rng = rand::rng();
        let mut words = [""; WORDS];
        for word in &mut words {
            *word = list.choose(&mut rng).ok_or(CodeError::EmptyWordlist)?;
        }
        Ok(Self { nameplate, words })
    }

    /// Picks a random nameplate that is not in `taken`, if one is left.
    #[must_use]
    pub fn free_nameplate(taken: &[u8]) -> Option<u8> {
        let free: Vec<u8> = (1..=MAX_NAMEPLATE).filter(|n| !taken.contains(n)).collect();
        free.choose(&mut rand::rng()).copied()
    }

    /// The public nameplate, if this is a target-shown code.
    #[must_use]
    pub fn nameplate(&self) -> Option<u8> {
        self.nameplate
    }

    /// Parses a typed code. Any run of characters other than letters, digits
    /// and `;` separates tokens, so `42 acid acorn acre` and `42-acid-acorn-acre`
    /// are the same code. A leading all-digit token is the nameplate.
    ///
    /// # Errors
    ///
    /// If the nameplate is out of range, the number of words is wrong, or a
    /// word is unknown or ambiguous.
    pub fn parse(input: &str) -> Result<Self, CodeError> {
        let mut tokens: Vec<&str> = input
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == ';'))
            .filter(|t| !t.is_empty())
            .collect();

        let nameplate = match tokens.first() {
            Some(first) if first.bytes().all(|b| b.is_ascii_digit()) => {
                let n = first
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (1..=MAX_NAMEPLATE).contains(n))
                    .ok_or_else(|| CodeError::Nameplate((*first).to_string()))?;
                tokens.remove(0);
                Some(n)
            }
            _ => None,
        };

        if tokens.len() != WORDS {
            return Err(CodeError::WordCount(tokens.len()));
        }
        let mut words = [""; WORDS];
        for (slot, token) in words.iter_mut().zip(tokens) {
            *slot = normalise_word(token)?;
        }
        Ok(Self { nameplate, words })
    }

    /// The PAKE password: the canonical spelling of the whole code.
    #[must_use]
    pub fn password(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(n) = self.nameplate {
            write!(f, "{n}-")?;
        }
        f.write_str(&self.words.join("-"))
    }
}

/// Keymap swaps between what a user meant and what a US console produced.
/// Each is its own inverse.
fn qwertz(c: char) -> char {
    match c {
        'y' => 'z',
        'z' => 'y',
        c => c,
    }
}

fn azerty(c: char) -> char {
    match c {
        'a' => 'q',
        'q' => 'a',
        'z' => 'w',
        'w' => 'z',
        'm' => ';',
        ';' => 'm',
        c => c,
    }
}

/// `word` as typed on a US, a QWERTZ and an AZERTY keyboard while the
/// console uses a US keymap. Every swap is its own inverse, so these are
/// also the spellings to try for a typed word.
#[must_use]
pub fn layout_variants(typed: &str) -> [String; 3] {
    [
        typed.to_string(),
        typed.chars().map(qwertz).collect(),
        typed.chars().map(azerty).collect(),
    ]
}

/// The word a typed token means, undoing keymap mix-ups.
///
/// # Errors
///
/// If no spelling of the token is a code word, or several are.
pub fn normalise_word(token: &str) -> Result<&'static str, CodeError> {
    let typed = token.to_ascii_lowercase();
    let mut found: Vec<&'static str> = layout_variants(&typed)
        .iter()
        .filter_map(|c| lookup(c))
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [word] => Ok(word),
        [] => Err(CodeError::UnknownWord(token.to_string())),
        many => Err(CodeError::AmbiguousWord(token.to_string(), many.join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
