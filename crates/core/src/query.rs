//! Query language, a practical subset of Everything's search syntax:
//!
//! | syntax              | meaning                                             |
//! |---------------------|-----------------------------------------------------|
//! | `foo bar`           | name contains `foo` AND `bar` (case-insensitive)    |
//! | `foo\|bar`          | OR (binds tighter than AND, like Everything)        |
//! | `!foo`              | NOT                                                 |
//! | `"a b"`             | literal containing spaces                           |
//! | `*.rs`, `a?c`       | wildcards match the whole name                      |
//! | `users\me`          | a `\` matches against the full path                 |
//! | `case:Foo`          | case-sensitive                                      |
//! | `path:foo`          | match `foo` against the full path                   |
//! | `ww:foo`            | whole word                                          |
//! | `regex:^a.*z$`      | regular expression (case-insensitive unless `case:`) |
//! | `ext:jpg;png`       | extension list                                      |
//! | `size:>10mb`        | size compare (`<`, `<=`, `>`, `>=`, `=`, `a..b`)    |
//! | `file:` `folder:`   | only files / only folders                           |

use crate::fold::fold;
use crate::index::Index;

#[derive(Debug)]
pub enum Node {
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
}

#[derive(Debug)]
pub enum Leaf {
    /// Substring. `needle` is folded unless `case`.
    Text { needle: Vec<u8>, case: bool, path: bool, whole_word: bool },
    /// Whole-name (or whole-path) wildcard pattern.
    Glob { pat: Vec<u8>, case: bool, path: bool },
    Regex { re: regex::bytes::Regex, path: bool },
    /// Folded extensions without the dot.
    Ext(Vec<Vec<u8>>),
    Size { lo: u64, hi: u64 },
    IsFile,
    IsDir,
}

#[derive(Debug)]
pub struct Query {
    pub root: Node,
}

#[derive(Default, Clone, Copy)]
struct Mods {
    case: bool,
    path: bool,
    ww: bool,
    regex: bool,
}

/// Splits on whitespace outside double quotes. Quotes are kept in the token
/// so `"a|b"` is not split on `|` later.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Splits a token on `|` outside quotes.
fn split_or(tok: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut quoted = false;
    for c in tok.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                parts.last_mut().unwrap().push(c);
            }
            '|' if !quoted => parts.push(String::new()),
            c => parts.last_mut().unwrap().push(c),
        }
    }
    parts
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = [("tb", 1u64 << 40), ("gb", 1 << 30), ("mb", 1 << 20), ("kb", 1 << 10), ("b", 1)]
        .iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n.to_string(), *m)))
        .unwrap_or((s.clone(), 1));
    let v: f64 = num.trim().parse().ok()?;
    Some((v * mul as f64) as u64)
}

fn parse_size_expr(v: &str) -> Option<Leaf> {
    let (lo, hi) = if let Some((a, b)) = v.split_once("..") {
        (parse_size(a)?, parse_size(b)?)
    } else if let Some(x) = v.strip_prefix(">=") {
        (parse_size(x)?, u64::MAX)
    } else if let Some(x) = v.strip_prefix("<=") {
        (0, parse_size(x)?)
    } else if let Some(x) = v.strip_prefix('>') {
        (parse_size(x)?.saturating_add(1), u64::MAX)
    } else if let Some(x) = v.strip_prefix('<') {
        (0, parse_size(x)?.saturating_sub(1))
    } else {
        let x = parse_size(v.strip_prefix('=').unwrap_or(v))?;
        (x, x)
    };
    Some(Leaf::Size { lo, hi })
}

fn unquote(s: &str) -> String {
    s.replace('"', "")
}

/// Parses one OR-alternative (may carry `!` and modifiers).
fn parse_atom(mut s: &str, mut m: Mods) -> Result<Option<Node>, String> {
    let mut neg = false;
    while let Some(rest) = s.strip_prefix('!') {
        neg = !neg;
        s = rest;
    }
    let mut filters: Vec<Node> = Vec::new();
    // Modifier prefixes, possibly stacked: `case:path:foo`.
    loop {
        let Some((key, rest)) = s.split_once(':') else { break };
        if key.starts_with('"') {
            break;
        }
        match key.to_ascii_lowercase().as_str() {
            "case" => m.case = true,
            "nocase" => m.case = false,
            "path" => m.path = true,
            "nopath" => m.path = false,
            "ww" | "wholeword" => m.ww = true,
            "regex" => {
                m.regex = true;
                s = rest;
                break; // the rest is the pattern verbatim
            }
            "file" | "files" => filters.push(Node::Leaf(Leaf::IsFile)),
            "folder" | "folders" => filters.push(Node::Leaf(Leaf::IsDir)),
            "ext" => {
                let exts = unquote(rest)
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(|e| fold(e.trim_start_matches('.')).into_bytes())
                    .collect();
                filters.push(Node::Leaf(Leaf::Ext(exts)));
                s = "";
                break;
            }
            "size" => {
                filters.push(Node::Leaf(parse_size_expr(rest).ok_or_else(|| format!("bad size: {rest}"))?));
                s = "";
                break;
            }
            _ => break, // not a modifier (e.g. `c:\windows`)
        }
        s = rest;
    }

    let text = unquote(s);
    if !text.is_empty() {
        let path = m.path || text.contains('\\');
        let leaf = if m.regex {
            let re = regex::bytes::RegexBuilder::new(&text)
                .case_insensitive(!m.case)
                .build()
                .map_err(|e| e.to_string())?;
            Leaf::Regex { re, path }
        } else if text.contains(['*', '?']) {
            let pat = if m.case { text.into_bytes() } else { fold(&text).into_bytes() };
            Leaf::Glob { pat, case: m.case, path }
        } else {
            let needle = if m.case { text.into_bytes() } else { fold(&text).into_bytes() };
            Leaf::Text { needle, case: m.case, path, whole_word: m.ww }
        };
        filters.push(Node::Leaf(leaf));
    }

    let node = match filters.len() {
        0 => return Ok(None),
        1 => filters.pop().unwrap(),
        _ => Node::All(filters),
    };
    Ok(Some(if neg { Node::Not(Box::new(node)) } else { node }))
}

impl Query {
    pub fn parse(s: &str) -> Result<Query, String> {
        let toks = tokenize(s);
        // Join standalone `|` tokens: `a | b` → `a|b`.
        let mut joined: Vec<String> = Vec::new();
        let mut glue = false;
        for t in toks {
            if t == "|" {
                glue = true;
                continue;
            }
            let attach = glue || t.starts_with('|') || joined.last().is_some_and(|l| l.ends_with('|'));
            if attach && let Some(last) = joined.last_mut() {
                if !last.ends_with('|') {
                    last.push('|');
                }
                last.push_str(t.trim_start_matches('|'));
            } else {
                joined.push(t);
            }
            glue = false;
        }

        let mut all = Vec::new();
        for t in joined {
            let alts: Vec<Node> = split_or(&t)
                .iter()
                .filter(|a| !a.is_empty())
                .map(|a| parse_atom(a, Mods::default()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
            match alts.len() {
                0 => {}
                1 => all.extend(alts),
                _ => all.push(Node::Any(alts)),
            }
        }
        Ok(Query { root: Node::All(all) })
    }

    pub fn is_empty(&self) -> bool {
        matches!(&self.root, Node::All(v) if v.is_empty())
    }

    /// True when the whole query is one case-insensitive name substring.
    pub fn is_single_name_substring(&self) -> bool {
        matches!(&self.root, Node::All(v) if v.len() == 1
            && matches!(&v[0], Node::Leaf(Leaf::Text { case: false, path: false, whole_word: false, .. })))
    }

    /// A folded literal that every match must contain in its *name*, used to
    /// drive the arena scan. Picks the longest available.
    pub fn driver(&self) -> Option<Vec<u8>> {
        let Node::All(items) = &self.root else { return None };
        items
            .iter()
            .filter_map(|n| match n {
                Node::Leaf(Leaf::Text { needle, case: false, path: false, .. }) => Some(needle.clone()),
                Node::Leaf(Leaf::Glob { pat, case: false, path: false }) => pat
                    .split(|&b| b == b'*' || b == b'?')
                    .max_by_key(|p| p.len())
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_vec()),
                Node::Leaf(Leaf::Ext(exts)) if exts.len() == 1 => {
                    let mut v = vec![b'.'];
                    v.extend_from_slice(&exts[0]);
                    Some(v)
                }
                _ => None,
            })
            .max_by_key(|v| v.len())
    }
}

// ---- evaluation --------------------------------------------------------------

/// Per-entry evaluation context; builds paths lazily and at most once.
pub struct Ctx<'a> {
    pub ix: &'a Index,
    pub i: u32,
    fold_path: Option<Vec<u8>>,
    raw_path: Option<Vec<u8>>,
}

impl<'a> Ctx<'a> {
    pub fn new(ix: &'a Index, i: u32) -> Self {
        Ctx { ix, i, fold_path: None, raw_path: None }
    }
    fn fold_path(&mut self) -> &[u8] {
        if self.fold_path.is_none() {
            let mut v = Vec::with_capacity(128);
            self.ix.fold_path_into(self.i, &mut v);
            self.fold_path = Some(v);
        }
        self.fold_path.as_deref().unwrap()
    }
    fn raw_path(&mut self) -> &[u8] {
        if self.raw_path.is_none() {
            self.raw_path = Some(self.ix.path(self.i).into_bytes());
        }
        self.raw_path.as_deref().unwrap()
    }
    fn hay(&mut self, path: bool, case: bool) -> &[u8] {
        match (path, case) {
            (false, false) => self.ix.fold_name(self.i),
            (false, true) => self.ix.name(self.i).as_bytes(),
            (true, false) => self.fold_path(),
            (true, true) => self.raw_path(),
        }
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn contains_word(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find_iter(hay, needle).any(|p| {
        let before = p == 0 || !is_word(hay[p - 1]);
        let end = p + needle.len();
        let after = end == hay.len() || !is_word(hay[end]);
        before && after
    })
}

/// Wildcard match over UTF-8 bytes; `?` consumes one character.
pub fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star_p, mut star_i) = (usize::MAX, 0usize);
    let char_len = |s: &[u8], i: usize| {
        let mut j = i + 1;
        while j < s.len() && s[j] & 0xC0 == 0x80 {
            j += 1;
        }
        j - i
    };
    while i < s.len() {
        if p < pat.len() && pat[p] == b'?' {
            p += 1;
            i += char_len(s, i);
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = p;
            star_i = i;
            p += 1;
        } else if p < pat.len() && pat[p] == s[i] {
            p += 1;
            i += 1;
        } else if star_p != usize::MAX {
            p = star_p + 1;
            star_i += char_len(s, star_i);
            i = star_i;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

impl Node {
    pub fn eval(&self, c: &mut Ctx) -> bool {
        match self {
            Node::All(v) => v.iter().all(|n| n.eval(c)),
            Node::Any(v) => v.iter().any(|n| n.eval(c)),
            Node::Not(n) => !n.eval(c),
            Node::Leaf(l) => l.eval(c),
        }
    }
}

impl Leaf {
    fn eval(&self, c: &mut Ctx) -> bool {
        match self {
            Leaf::Text { needle, case, path, whole_word } => {
                let hay = c.hay(*path, *case);
                if *whole_word {
                    contains_word(hay, needle)
                } else {
                    memchr::memmem::find(hay, needle).is_some()
                }
            }
            Leaf::Glob { pat, case, path } => glob_match(pat, c.hay(*path, *case)),
            Leaf::Regex { re, path } => re.is_match(c.hay(*path, true)),
            Leaf::Ext(exts) => {
                let name = c.ix.fold_name(c.i);
                !c.ix.entries[c.i as usize].is_dir()
                    && exts.iter().any(|e| {
                        name.len() > e.len() && name.ends_with(e) && name[name.len() - e.len() - 1] == b'.'
                    })
            }
            Leaf::Size { lo, hi } => {
                let e = &c.ix.entries[c.i as usize];
                !e.is_dir() && e.size >= *lo && e.size <= *hi
            }
            Leaf::IsFile => !c.ix.entries[c.i as usize].is_dir(),
            Leaf::IsDir => c.ix.entries[c.i as usize].is_dir(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn matches(q: &str) -> Vec<String> {
        let ix = sample();
        let q = Query::parse(q).unwrap();
        let mut v: Vec<String> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_live(i) && q.root.eval(&mut Ctx::new(&ix, i)))
            .map(|i| ix.name(i).to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn substring_and_or_not() {
        assert_eq!(matches("note"), vec!["notepad.exe", "notes.txt"]);
        assert_eq!(matches("note !exe"), vec!["notes.txt"]);
        assert_eq!(matches("report|notepad"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("report | notepad"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("case:report"), Vec::<String>::new());
        assert_eq!(matches("case:Report"), vec!["Report.PDF"]);
    }

    #[test]
    fn wildcards_paths_and_filters() {
        assert_eq!(matches("*.txt"), vec!["notes.txt"]);
        assert_eq!(matches("n?tes.*"), vec!["notes.txt"]);
        assert_eq!(matches(r"users\me\"), vec!["Report.PDF", "notes.txt"]);
        assert_eq!(matches("ext:pdf;exe"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("size:>100"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("size:1kb..2kb"), Vec::<String>::new());
        assert_eq!(matches("folder: e"), vec!["Me", "Users"]);
        assert_eq!(matches("regex:^no.*t$"), vec!["notes.txt"]);
        assert_eq!(matches("ww:me"), vec!["Me"]);
        assert_eq!(matches(r#""notes.txt""#), vec!["notes.txt"]);
    }

    #[test]
    fn driver_picks_longest_literal() {
        assert_eq!(Query::parse("ab *.longer").unwrap().driver(), Some(b".longer".to_vec()));
        assert_eq!(Query::parse("ext:rs").unwrap().driver(), Some(b".rs".to_vec()));
        assert_eq!(Query::parse("a|b").unwrap().driver(), None);
    }

    #[test]
    fn glob_handles_unicode_question_mark() {
        assert!(glob_match("h?j".as_bytes(), "hå j".replace(' ', "").as_bytes()));
        assert!(glob_match(b"*", b""));
        assert!(!glob_match(b"a*b", b"ac"));
    }
}
