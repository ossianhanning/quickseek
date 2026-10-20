//! Which parts of a displayed name/path match the search terms.

use rapidfind_core::fold::fold_into;

/// Byte ranges `[start, end)` of `text` that match any of the (folded)
/// `terms`, merged and sorted. Matching is done on the folded text and mapped
/// back to whole characters of the original.
pub fn ranges(text: &str, terms: &[Vec<u8>]) -> Vec<(usize, usize)> {
    if terms.is_empty() || text.is_empty() {
        return Vec::new();
    }
    // Folded bytes, and for each folded byte the original char's span.
    let mut folded = Vec::with_capacity(text.len());
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    let mut tmp = String::new();
    for (o, c) in text.char_indices() {
        tmp.clear();
        tmp.push(c);
        let before = folded.len();
        fold_into(&tmp, &mut folded);
        for _ in before..folded.len() {
            span.push((o, o + c.len_utf8()));
        }
    }
    let mut out: Vec<(usize, usize)> = Vec::new();
    for t in terms.iter().filter(|t| !t.is_empty()) {
        for p in memchr::memmem::find_iter(&folded, t) {
            out.push((span[p].0, span[p + t.len() - 1].1));
        }
    }
    out.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(out.len());
    for (s, e) in out {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Splits `text` into (segment, highlighted) runs.
pub fn segments<'a>(text: &'a str, ranges: &[(usize, usize)]) -> Vec<(&'a str, bool)> {
    let mut out = Vec::new();
    let mut pos = 0;
    for &(s, e) in ranges {
        if s > pos {
            out.push((&text[pos..s], false));
        }
        out.push((&text[s..e], true));
        pos = e;
    }
    if pos < text.len() {
        out.push((&text[pos..], false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Vec<Vec<u8>> {
        s.split(' ').map(|x| x.as_bytes().to_vec()).collect()
    }

    #[test]
    fn finds_case_insensitive_and_merges() {
        assert_eq!(ranges("Report.PDF", &t("report pdf")), vec![(0, 6), (7, 10)]);
        assert_eq!(ranges("aaaa", &t("aa")), vec![(0, 4)]);
        assert_eq!(ranges("notes", &t("x")), vec![]);
        assert_eq!(segments("Report.PDF", &[(0, 6)]), vec![("Report", true), (".PDF", false)]);
    }

    #[test]
    fn maps_back_through_unicode_folding() {
        // "Å" folds to "å" (same length in chars, different bytes per case).
        let r = ranges("ÅSA Ö.txt", &t("åsa ö"));
        assert_eq!(segments("ÅSA Ö.txt", &r), vec![("ÅSA", true), (" ", false), ("Ö", true), (".txt", false)]);
        // İ lowercases to two chars; the whole original char is highlighted.
        let r = ranges("xİy", &["i".as_bytes().to_vec()]);
        assert_eq!(segments("xİy", &r), vec![("x", false), ("İ", true), ("y", false)]);
    }
}
