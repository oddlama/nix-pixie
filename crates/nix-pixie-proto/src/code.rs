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

use rand::RngExt;

/// Number of secret words in a code.
pub const WORDS: usize = 3;
/// Nameplates are chosen from `1..=MAX_NAMEPLATE`.
pub const MAX_NAMEPLATE: u8 = 99;

const WORDLIST_SOURCE: &str = include_str!("wordlist.txt");

/// The pairing wordlist, in file order.
pub fn wordlist() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        WORDLIST_SOURCE
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .collect()
    })
}

fn word_index(word: &str) -> Option<u16> {
    // The list is sorted; the tests check that.
    wordlist()
        .binary_search(&word)
        .ok()
        .map(|i| u16::try_from(i).expect("wordlist has fewer than 65536 words"))
}

/// A parsed or generated pairing code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code {
    nameplate: Option<u8>,
    words: [u16; WORDS],
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
}

impl Code {
    /// Generates a code with fresh random words.
    pub fn generate(nameplate: Option<u8>) -> Self {
        let mut rng = rand::rng();
        let len = u16::try_from(wordlist().len()).expect("wordlist has fewer than 65536 words");
        Self {
            nameplate,
            words: std::array::from_fn(|_| rng.random_range(0..len)),
        }
    }

    /// Picks a random nameplate that is not in `taken`, if one is left.
    pub fn free_nameplate(taken: &[u8]) -> Option<u8> {
        let free: Vec<u8> = (1..=MAX_NAMEPLATE).filter(|n| !taken.contains(n)).collect();
        if free.is_empty() {
            None
        } else {
            Some(free[rand::rng().random_range(0..free.len())])
        }
    }

    /// The public nameplate, if this is a target-shown code.
    pub fn nameplate(&self) -> Option<u8> {
        self.nameplate
    }

    /// Parses a typed code. Any run of characters other than letters, digits
    /// and `;` separates tokens, so `42 acid acorn acre` and `42-acid-acorn-acre`
    /// are the same code. A leading all-digit token is the nameplate.
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
        let mut words = [0; WORDS];
        for (slot, token) in words.iter_mut().zip(tokens) {
            *slot = normalise_word(token)?;
        }
        Ok(Self { nameplate, words })
    }

    /// The PAKE password: the canonical spelling of the whole code.
    pub fn password(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(n) = self.nameplate {
            write!(f, "{n}-")?;
        }
        let list = wordlist();
        let words: Vec<&str> = self.words.iter().map(|&i| list[usize::from(i)]).collect();
        f.write_str(&words.join("-"))
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

/// Spellings to try for a typed word: as typed, and with each layout undone.
fn candidates(typed: &str) -> [String; 3] {
    [
        typed.to_string(),
        typed.chars().map(qwertz).collect(),
        typed.chars().map(azerty).collect(),
    ]
}

fn normalise_word(token: &str) -> Result<u16, CodeError> {
    let typed = token.to_ascii_lowercase();
    let mut found: Vec<u16> = candidates(&typed)
        .iter()
        .filter_map(|c| word_index(c))
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [index] => Ok(*index),
        [] => Err(CodeError::UnknownWord(token.to_string())),
        many => {
            let list = wordlist();
            let names: Vec<&str> = many.iter().map(|&i| list[usize::from(i)]).collect();
            Err(CodeError::AmbiguousWord(
                token.to_string(),
                names.join(", "),
            ))
        }
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
        for (i, word) in wordlist().iter().enumerate() {
            let typed_as = [
                word.to_string(),
                word.chars().map(qwertz).collect::<String>(),
                word.chars().map(azerty).collect::<String>(),
            ];
            for typed in typed_as {
                assert_eq!(
                    normalise_word(&typed),
                    Ok(u16::try_from(i).unwrap()),
                    "{word} typed as {typed}"
                );
            }
        }
    }

    #[test]
    fn round_trip() {
        for nameplate in [None, Some(1), Some(42), Some(MAX_NAMEPLATE)] {
            let code = Code::generate(nameplate);
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
