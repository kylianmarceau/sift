//! Text folding shared by indexed names and queries.
//!
//! Both sides go through the same function so matching can compare raw
//! bytes: lowercase, strip Latin combining diacritics, then NFC. That makes
//! search case- and accent-insensitive (`cafe` finds `Café.txt`) and makes
//! decomposed (NFD) names, which older macOS filesystems produce, match
//! queries typed in the usual composed form.

use unicode_normalization::UnicodeNormalization;

/// Appends the folded form of `raw` to `out`.
///
/// Invalid UTF-8 is replaced with U+FFFD; the raw bytes remain in the
/// database for display and opening.
pub fn fold_into(raw: &[u8], out: &mut Vec<u8>) {
    if raw.is_ascii() {
        out.extend(raw.iter().map(u8::to_ascii_lowercase));
        return;
    }
    let text = String::from_utf8_lossy(raw).to_lowercase();
    let mut buf = [0u8; 4];
    for c in text.nfd().filter(|c| !is_latin_diacritic(*c)).nfc() {
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

pub fn fold(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    fold_into(raw, &mut out);
    out
}

/// Combining Diacritical Marks block. Marks from other scripts (for
/// example Japanese dakuten) change meaning and are kept.
fn is_latin_diacritic(c: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&c)
}

/// A 64-bit summary of which characters occur in a folded string.
///
/// If `bag(query) & !bag(name) != 0`, the name lacks a character the query
/// needs and cannot match, so most names are rejected with one AND before
/// any real matching work.
pub fn char_bag(folded: &[u8]) -> u64 {
    folded.iter().fold(0u64, |bag, &b| bag | char_bit(b))
}

fn char_bit(b: u8) -> u64 {
    let bit = match b {
        b'a'..=b'z' => b - b'a',
        b'0'..=b'9' => 26 + (b - b'0'),
        b'.' => 36,
        b'-' => 37,
        b'_' => 38,
        b' ' => 39,
        0x80.. => 40,
        _ => 41,
    };
    1 << bit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> String {
        String::from_utf8(fold(s.as_bytes())).unwrap()
    }

    #[test]
    fn folds_case_and_accents() {
        assert_eq!(f("Project.RS"), "project.rs");
        assert_eq!(f("Café Menu"), "cafe menu");
        assert_eq!(f("cafe\u{301}"), "cafe");
        assert_eq!(f("ÉCOLE"), "ecole");
        assert_eq!(f("Straße"), "straße");
    }

    #[test]
    fn keeps_non_latin_scripts() {
        assert_eq!(f("日本語"), "日本語");
        // Dakuten survive, and decomposed input composes: か + ゙ = が
        assert_eq!(f("か\u{3099}"), "が");
        assert_eq!(f("Привет"), "привет");
    }

    #[test]
    fn replaces_invalid_utf8() {
        assert_eq!(fold(b"bad\xffname"), "bad\u{fffd}name".as_bytes());
    }

    #[test]
    fn bag_detects_missing_characters() {
        let name = char_bag(b"report-2024.pdf");
        assert_eq!(char_bag(b"rpt") & !name, 0);
        assert_eq!(char_bag(b"2024") & !name, 0);
        assert_ne!(char_bag(b"rpz") & !name, 0);
    }
}
