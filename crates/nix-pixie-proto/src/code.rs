//! Pairing codes: an optional public nameplate plus three secret words.
//!
//! * Target-shown codes (the default pairing mode) look like
//!   `482913-acid-acorn-acre`, or `42-acid-acorn-acre` for a target that
//!   doesn't use relays. The nameplate is public: the target advertises it
//!   so the controller knows which machine to dial (see [`NameplateSpace`]).
//!   Only the words are secret.
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
use std::ops::RangeInclusive;
use std::sync::OnceLock;

use rand::seq::IndexedRandom;

/// Number of secret words in a code.
pub const WORDS: usize = 3;

/// Which range a target's nameplate comes from. The nameplate only has to
/// tell apart the targets a controller can see at once, and that depends on
/// where it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameplateSpace {
    /// `1..=99`, for targets found on their local network only. A target
    /// picks one no other target there advertises.
    Local,
    /// Six digits, for targets that can also be found through a relay (see
    /// [`crate::rendezvous`]). Everybody using that relay shares these, and
    /// nobody can list the ones in use, so there are enough that two targets
    /// picking the same one at random is unlikely (1 in 900,000 per pair).
    Relay,
}

impl NameplateSpace {
    /// The nameplates of this space.
    #[must_use]
    pub const fn range(self) -> RangeInclusive<u32> {
        match self {
            Self::Local => 1..=99,
            Self::Relay => 100_000..=999_999,
        }
    }

    /// The space `nameplate` belongs to, if it is a nameplate at all.
    #[must_use]
    pub fn of(nameplate: u32) -> Option<Self> {
        [Self::Local, Self::Relay]
            .into_iter()
            .find(|space| space.range().contains(&nameplate))
    }
}

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

/// The words starting with `prefix` (in any case), for completing a code
/// while it is typed. The list is public, so this gives nothing away.
#[must_use]
pub fn completions(prefix: &str) -> &'static [&'static str] {
    let prefix = prefix.to_ascii_lowercase();
    let list = wordlist();
    // The list is sorted; the tests check that.
    let start = list.partition_point(|word| *word < prefix.as_str());
    let rest = list.get(start..).unwrap_or_default();
    let end = start + rest.partition_point(|word| word.starts_with(&prefix));
    list.get(start..end).unwrap_or_default()
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
    nameplate: Option<u32>,
    /// Words from [`wordlist`], in order.
    words: [&'static str; WORDS],
}

/// Why a typed code was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodeError {
    #[error("expected {WORDS} words, got {0}")]
    WordCount(usize),
    #[error("`{0}` is not a valid nameplate (1-99, or six digits)")]
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
    pub fn generate(nameplate: Option<u32>) -> Result<Self, CodeError> {
        let list = wordlist();
        let mut rng = rand::rng();
        let mut words = [""; WORDS];
        for word in &mut words {
            *word = list.choose(&mut rng).ok_or(CodeError::EmptyWordlist)?;
        }
        Ok(Self { nameplate, words })
    }

    /// Picks a random nameplate of `space` that is not in `taken`, if one is
    /// left.
    #[must_use]
    pub fn free_nameplate(space: NameplateSpace, taken: &[u32]) -> Option<u32> {
        match space {
            NameplateSpace::Local => {
                let free: Vec<u32> = space.range().filter(|n| !taken.contains(n)).collect();
                free.choose(&mut rand::rng()).copied()
            }
            // Too many to list, and `taken` (what the local network shows)
            // is a handful at most: draw until one is free.
            NameplateSpace::Relay => (0..64)
                .map(|_| rand::random_range(space.range()))
                .find(|n| !taken.contains(n)),
        }
    }

    /// The public nameplate, if this is a target-shown code.
    #[must_use]
    pub fn nameplate(&self) -> Option<u32> {
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
                    .parse::<u32>()
                    .ok()
                    .filter(|n| NameplateSpace::of(*n).is_some())
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
