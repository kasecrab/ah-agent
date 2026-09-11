//! The pairing code as something a person can read off a screen and type.
//!
//! Base32 rather than base64: no case to get wrong, and the alphabet has no
//! `0` or `1` in it, so a zero typed where an `O` was shown can be put back
//! without guessing. Grouped in fours because a run of 32 characters is easy
//! to lose your place in.

use crate::crypto::CODE_BYTES;
use data_encoding::BASE32_NOPAD;

/// Characters per group, between dashes.
const GROUP: usize = 4;

/// The code as it is shown: 32 characters in 8 groups.
pub fn format(code: &[u8; CODE_BYTES]) -> String {
    let raw = BASE32_NOPAD.encode(code);
    let mut out = String::with_capacity(raw.len() + raw.len() / GROUP);
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && i % GROUP == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

/// The code as it was typed. Dashes and spaces are ignored, case is ignored,
/// and the two characters that are not in the alphabet at all are read as the
/// two that look like them.
pub fn parse(typed: &str) -> Option<[u8; CODE_BYTES]> {
    let mut clean = String::with_capacity(32);
    for c in typed.chars() {
        match c {
            '-' | ' ' | '\t' | '\u{2013}' | '\u{2014}' => continue,
            // Neither is in base32, so neither can be what was meant, and
            // each has exactly one letter it could have been read from.
            '0' => clean.push('O'),
            '1' => clean.push('I'),
            c if c.is_ascii_alphanumeric() => clean.push(c.to_ascii_uppercase()),
            _ => return None,
        }
    }
    let bytes = BASE32_NOPAD.decode(clean.as_bytes()).ok()?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> [u8; CODE_BYTES] {
        let mut c = [0u8; CODE_BYTES];
        for (i, b) in c.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        c
    }

    #[test]
    fn a_code_survives_the_grouping() {
        let c = code();
        let shown = format(&c);
        assert_eq!(shown.len(), 32 + 7, "32 characters in 8 groups: {shown}");
        assert_eq!(parse(&shown), Some(c));
    }

    #[test]
    fn the_dashes_are_only_for_reading() {
        let c = code();
        let shown = format(&c);
        assert_eq!(parse(&shown.replace('-', "")), Some(c));
        assert_eq!(parse(&shown.replace('-', " ")), Some(c));
    }

    #[test]
    fn case_is_not_part_of_the_code() {
        let c = code();
        assert_eq!(parse(&format(&c).to_lowercase()), Some(c));
    }

    #[test]
    fn a_zero_typed_for_an_o_still_pairs() {
        let raw = BASE32_NOPAD.encode(&code());
        if raw.contains('O') {
            assert_eq!(parse(&raw.replace('O', "0")), Some(code()));
        }
        if raw.contains('I') {
            assert_eq!(parse(&raw.replace('I', "1")), Some(code()));
        }
        // And with nothing to substitute, the substitution is still harmless:
        // neither character is in the alphabet, so it can never have been one.
        assert!(!BASE32_NOPAD.specification().symbols.contains('0'));
        assert!(!BASE32_NOPAD.specification().symbols.contains('1'));
    }

    #[test]
    fn something_that_is_not_a_code_is_not_read_as_one() {
        assert_eq!(parse(""), None, "empty");
        assert_eq!(parse("AAAA"), None, "too short");
        assert_eq!(parse(&format(&code()).repeat(2)), None, "too long");
        assert_eq!(parse("hello, world!"), None, "punctuation");
        assert_eq!(
            parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA8"),
            None,
            "not base32"
        );
    }

    #[test]
    fn every_code_round_trips() {
        // Walks the whole byte range through every position, since a codec is
        // exactly the kind of thing that works for the bytes you thought of.
        for seed in 0u16..=255 {
            let mut c = [0u8; CODE_BYTES];
            for (i, b) in c.iter_mut().enumerate() {
                *b = (seed as u8).wrapping_add(i as u8).wrapping_mul(31);
            }
            assert_eq!(parse(&format(&c)), Some(c), "seed {seed}");
        }
    }
}
