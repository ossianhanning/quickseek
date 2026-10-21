//! Case folding used for case-insensitive matching. Names are folded once at
//! index time so the hot search loop is a plain byte search.
//!
//! Two modes: plain lowercase, or lowercase with diacritics removed
//! (`é` → `e`, `å` → `a`), Everything's default "match diacritics: off".

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FoldMode {
    /// Lowercase only; diacritics are significant.
    #[default]
    Lower,
    /// Lowercase and strip combining marks.
    StripDiacritics,
}

pub fn fold_into_mode(s: &str, out: &mut Vec<u8>, mode: FoldMode) {
    if s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
        return;
    }
    let mut tmp = [0u8; 4];
    match mode {
        FoldMode::Lower => {
            for c in s.chars() {
                for l in c.to_lowercase() {
                    out.extend_from_slice(l.encode_utf8(&mut tmp).as_bytes());
                }
            }
        }
        FoldMode::StripDiacritics => {
            for c in s.chars() {
                if c.is_ascii() {
                    out.push(c.to_ascii_lowercase() as u8);
                    continue;
                }
                for d in std::iter::once(c).nfd().filter(|&d| !is_combining_mark(d)) {
                    for l in d.to_lowercase() {
                        out.extend_from_slice(l.encode_utf8(&mut tmp).as_bytes());
                    }
                }
            }
        }
    }
}

pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    fold_into_mode(s, out, FoldMode::Lower)
}

pub fn fold_mode(s: &str, mode: FoldMode) -> String {
    let mut v = Vec::with_capacity(s.len());
    fold_into_mode(s, &mut v, mode);
    // Lowercasing / dropping whole chars from valid UTF-8 yields valid UTF-8.
    unsafe { String::from_utf8_unchecked(v) }
}

pub fn fold(s: &str) -> String {
    fold_mode(s, FoldMode::Lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_and_unicode() {
        assert_eq!(fold("HeLLo.TXT"), "hello.txt");
        assert_eq!(fold("ÅÄÖ Straße"), "åäö straße");
    }

    #[test]
    fn strips_diacritics() {
        assert_eq!(
            fold_mode("Ångström Café NAÏVE", FoldMode::StripDiacritics),
            "angstrom cafe naive"
        );
        // Precomposed and decomposed input fold identically.
        assert_eq!(fold_mode("e\u{301}", FoldMode::StripDiacritics), "e");
        assert_eq!(fold_mode("日本語", FoldMode::StripDiacritics), "日本語");
    }
}
