//! Case folding used for case-insensitive matching. Names are folded once at
//! index time so the hot search loop is a plain byte search.

pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    if s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
        return;
    }
    let mut tmp = [0u8; 4];
    for c in s.chars() {
        for l in c.to_lowercase() {
            out.extend_from_slice(l.encode_utf8(&mut tmp).as_bytes());
        }
    }
}

pub fn fold(s: &str) -> String {
    let mut v = Vec::with_capacity(s.len());
    fold_into(s, &mut v);
    // Lowercasing valid UTF-8 char by char yields valid UTF-8.
    unsafe { String::from_utf8_unchecked(v) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn folds_ascii_and_unicode() {
        assert_eq!(super::fold("HeLLo.TXT"), "hello.txt");
        assert_eq!(super::fold("ÅÄÖ Straße"), "åäö straße");
    }
}
