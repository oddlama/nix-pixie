//! Lowercase hex, for key files.

pub fn encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(super::encode(&bytes), "0001abff");
        assert_eq!(super::decode("0001abff").as_deref(), Some(&bytes[..]));
        assert_eq!(super::decode("0001ABFF").as_deref(), Some(&bytes[..]));
        assert_eq!(super::decode("abc"), None);
        assert_eq!(super::decode("zz"), None);
        assert_eq!(super::decode("é0"), None);
    }
}
