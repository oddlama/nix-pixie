//! Lowercase hex, for key files and announcement ids.

/// Lowercase hex of `bytes`.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The bytes spelled by `text` (either case), or `None` unless every
/// character is a hex digit and they pair up. Stricter than
/// `u8::from_str_radix`, which also takes a leading `+`.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|&[high, low]| Some(nibble(high)? << 4 | nibble(low)?))
        .collect()
}

fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(super::encode(&bytes), "0001abff");
        assert_eq!(super::decode("0001abff").as_deref(), Some(&bytes[..]));
        assert_eq!(super::decode("0001ABFF").as_deref(), Some(&bytes[..]));
        assert_eq!(super::decode("").as_deref(), Some(&[][..]));
    }

    #[test]
    fn rejects_anything_but_digit_pairs() {
        for bad in ["abc", "zz", "é0", "+f", "-1", " f", "0x"] {
            assert_eq!(super::decode(bad), None, "{bad:?}");
        }
    }
}
