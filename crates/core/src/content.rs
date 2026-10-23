//! Content search: Everything's `content:`.
//!
//! The service only knows names, and it runs as SYSTEM, so file contents are
//! read by the client, with the searching user's own rights:
//!
//! 1. [`split_query`] takes the `content:` terms out of the query (they may
//!    only be AND-ed with the rest), and the service runs what is left.
//! 2. A [`Scan`] walks the matches in display order ([`Request::Snapshot`]),
//!    reads each file on a pool of threads and decides with [`ContentQuery`].
//! 3. The GUI tells the service which rows failed ([`Request::DropRows`]) as
//!    it goes, so its list shrinks to the matches while sorting and paging
//!    stay server-side. The CLI and MCP server just keep the matching rows.
//!
//! | term | |
//! |---|---|
//! | `content:foo` | the file contains `foo` (case-insensitive) |
//! | `case:content:Foo`, `ww:content:foo` | case-sensitive / whole word |
//! | `regex:content:fo+`, `content-regex:fo+` | a regular expression |
//! | `!content:foo` | doesn't contain `foo` |
//!
//! Text is decoded first: UTF-8 (with or without a BOM), UTF-16 LE/BE (with a
//! BOM, or detected from its zero bytes) and otherwise the ANSI code page.
//! Files with other NUL bytes are binary and skipped unless
//! [`ContentOptions::binary`]. Office documents and PDFs go through the
//! IFilter installed for their type (see [`crate::ifilter`]). Files are read
//! in 1 MiB chunks with a 64 KiB overlap, so a regex match longer than that
//! across a chunk boundary can be missed. Online-only cloud files are never
//! opened (that would download them).

use std::io::{self, Read};
use std::ops::Range;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::ipc::{Request, Response, Row};
use crate::query::{SearchFlags, Tok, tokenize_spans};

/// Modifier keys that start a content term.
const KEYS: &[&str] = &["content", "content-regex", "regexcontent"];

pub fn is_content_key(k: &str) -> bool {
    KEYS.contains(&k)
}

#[derive(Debug, Clone)]
pub struct ContentTerm {
    /// What the user typed (after the modifiers, unquoted).
    pub text: String,
    pub neg: bool,
    pub case: bool,
    pub regex: bool,
    pub whole_word: bool,
    re: regex::bytes::Regex,
}

/// All content terms of a query (AND-ed).
#[derive(Debug, Clone, Default)]
pub struct ContentQuery {
    pub terms: Vec<ContentTerm>,
}

const NOT_HERE: &str = "content: can only be combined with AND (not inside an OR or a NOT group)";

/// Splits `content:` terms off a query: returns the rest (for the service)
/// and the content terms, if any. The search flags' case, whole word and
/// regex defaults apply to content terms too.
pub fn split_query(q: &str, flags: SearchFlags) -> Result<(String, Option<ContentQuery>), String> {
    let toks = tokenize_spans(q);
    let n = toks.len();
    let mut close_of = vec![n; n];
    let mut stack = Vec::new();
    for (i, (t, _)) in toks.iter().enumerate() {
        match t {
            Tok::Open => stack.push(i),
            Tok::Close => {
                if let Some(j) = stack.pop() {
                    close_of[j] = i;
                }
            }
            _ => {}
        }
    }
    let is = |i: usize, t: Tok| toks.get(i).is_some_and(|(x, _)| *x == t);
    // Per open group: could a content term inside it be AND-ed with everything?
    let mut plain = Vec::new();
    let mut terms = Vec::new();
    let mut cut = Vec::new();
    for (i, (t, span)) in toks.iter().enumerate() {
        match t {
            Tok::Open => {
                let ok = !(i > 0 && (is(i - 1, Tok::Not) || is(i - 1, Tok::Or)))
                    && !is(close_of[i] + 1, Tok::Or);
                plain.push(ok && plain.last().copied().unwrap_or(true));
            }
            Tok::Close => {
                plain.pop();
            }
            Tok::Word(w) => {
                if let Some(term) = parse_term(w, flags)? {
                    let or_next = (i > 0 && is(i - 1, Tok::Or)) || is(i + 1, Tok::Or);
                    if or_next || !plain.last().copied().unwrap_or(true) {
                        return Err(NOT_HERE.into());
                    }
                    terms.push(term);
                    cut.push(span.clone());
                }
            }
            _ => {}
        }
    }
    if terms.is_empty() {
        return Ok((q.to_string(), None));
    }
    let mut rest = String::with_capacity(q.len());
    let mut at = 0;
    for r in cut {
        rest.push_str(&q[at..r.start]);
        rest.push(' ');
        at = r.end;
    }
    rest.push_str(&q[at..]);
    Ok((
        rest.split_whitespace().collect::<Vec<_>>().join(" "),
        Some(ContentQuery { terms }),
    ))
}

/// `[!][case:|ww:|regex:...]content:value` → a term; anything else → None.
fn parse_term(word: &str, flags: SearchFlags) -> Result<Option<ContentTerm>, String> {
    let mut s = word;
    let mut neg = false;
    while let Some(r) = s.strip_prefix('!') {
        neg = !neg;
        s = r;
    }
    let (mut case, mut ww, mut regex) = (flags.case, flags.whole_word, flags.regex);
    let value = loop {
        let Some((key, rest)) = s.split_once(':') else {
            return Ok(None);
        };
        if key.is_empty() || key.starts_with('"') {
            return Ok(None);
        }
        match key.to_ascii_lowercase().as_str() {
            "case" => case = true,
            "nocase" => case = false,
            "ww" | "wholeword" => ww = true,
            "noww" | "nowholeword" => ww = false,
            "regex" => regex = true,
            "noregex" => regex = false,
            "content" => break rest,
            "content-regex" | "regexcontent" => {
                regex = true;
                break rest;
            }
            _ => return Ok(None),
        }
        s = rest;
    };
    let text = value.replace('"', "");
    if text.is_empty() {
        return Err("content: needs something to search for, e.g. content:\"some text\"".into());
    }
    ContentTerm::new(&text, neg, case, regex, ww).map(Some)
}

impl ContentTerm {
    pub fn new(
        text: &str,
        neg: bool,
        case: bool,
        regex: bool,
        whole_word: bool,
    ) -> Result<ContentTerm, String> {
        let pat = if regex {
            text.to_string()
        } else {
            regex::escape(text)
        };
        let pat = if whole_word {
            format!(r"\b(?:{pat})\b")
        } else {
            pat
        };
        let re = regex::bytes::RegexBuilder::new(&pat)
            .case_insensitive(!case)
            .multi_line(true)
            .size_limit(32 << 20)
            .build()
            .map_err(|e| format!("content: {e}"))?;
        Ok(ContentTerm {
            text: text.into(),
            neg,
            case,
            regex,
            whole_word,
            re,
        })
    }
}

impl ContentQuery {
    /// A query of one term, for callers with a separate content argument.
    pub fn single(text: &str, flags: SearchFlags) -> Result<ContentQuery, String> {
        Ok(ContentQuery {
            terms: vec![ContentTerm::new(
                text,
                false,
                flags.case,
                flags.regex,
                flags.whole_word,
            )?],
        })
    }

    /// Byte ranges of the positive terms' matches in `text`, sorted, for
    /// highlighting.
    pub fn find_ranges(&self, text: &[u8], max: usize) -> Vec<Range<usize>> {
        let mut v: Vec<Range<usize>> = self
            .terms
            .iter()
            .filter(|t| !t.neg)
            .flat_map(|t| t.re.find_iter(text).take(max).map(|m| m.range()))
            .filter(|r| !r.is_empty())
            .collect();
        v.sort_by_key(|r| (r.start, r.end));
        v.truncate(max);
        v
    }

    /// A one-line description, e.g. `"foo" and not "bar"`.
    pub fn describe(&self) -> String {
        self.terms
            .iter()
            .map(|t| format!("{}\"{}\"", if t.neg { "not " } else { "" }, t.text))
            .collect::<Vec<_>>()
            .join(" and ")
    }
}

// ---- decoding ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    /// The ANSI code page (not valid UTF-8, no NULs).
    Ansi,
    Binary,
}

/// How much of the start is used to tell text from binary.
const SNIFF: usize = 8 << 10;

/// The encoding of a file starting with `head`, and its BOM length.
pub fn detect(head: &[u8]) -> (Encoding, usize) {
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (Encoding::Utf8, 3);
    }
    if head.starts_with(&[0xFF, 0xFE]) {
        return (Encoding::Utf16Le, 2);
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        return (Encoding::Utf16Be, 2);
    }
    let sniff = &head[..head.len().min(SNIFF)];
    if memchr::memchr(0, sniff).is_some() {
        // UTF-16 without a BOM: mostly-ASCII text has a zero in every other byte.
        let pairs = sniff.len() / 2;
        let zeros = |odd: usize| {
            sniff
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|p| p[odd] == 0)
                .count()
        };
        let (lo, hi) = (zeros(0), zeros(1));
        if pairs >= 2 && hi * 10 >= pairs * 3 && lo * 20 <= pairs {
            return (Encoding::Utf16Le, 0);
        }
        if pairs >= 2 && lo * 10 >= pairs * 3 && hi * 20 <= pairs {
            return (Encoding::Utf16Be, 0);
        }
        return (Encoding::Binary, 0);
    }
    match std::str::from_utf8(head) {
        Ok(_) => (Encoding::Utf8, 0),
        Err(e) if e.error_len().is_none() => (Encoding::Utf8, 0), // cut mid-character
        Err(_) => (Encoding::Ansi, 0),
    }
}

/// Streaming decoder to UTF-8 (bytes split across chunks are carried over).
struct Decoder {
    enc: Encoding,
    carry: Vec<u8>,
}

impl Decoder {
    fn decode(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        match self.enc {
            Encoding::Utf8 | Encoding::Binary => out.extend_from_slice(raw),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let mut data = std::mem::take(&mut self.carry);
                data.extend_from_slice(raw);
                let be = self.enc == Encoding::Utf16Be;
                let mut units: Vec<u16> = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| {
                        if be {
                            u16::from_be_bytes([p[0], p[1]])
                        } else {
                            u16::from_le_bytes([p[0], p[1]])
                        }
                    })
                    .collect();
                let mut keep = data.len() % 2;
                // A high surrogate at the end waits for its partner.
                if units.last().is_some_and(|&u| (0xD800..0xDC00).contains(&u)) {
                    units.pop();
                    keep += 2;
                }
                self.carry = data[data.len() - keep..].to_vec();
                push_utf16(&units, out);
            }
            Encoding::Ansi => {
                let mut data = std::mem::take(&mut self.carry);
                data.extend_from_slice(raw);
                let cut = ansi_complete(&data);
                self.carry = data[cut..].to_vec();
                ansi_to_utf8(&data[..cut], out);
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.carry.is_empty() {
            let rest = std::mem::take(&mut self.carry);
            match self.enc {
                Encoding::Ansi => ansi_to_utf8(&rest, out),
                _ => out.extend_from_slice("\u{FFFD}".as_bytes()),
            }
        }
    }
}

fn push_utf16(units: &[u16], out: &mut Vec<u8>) {
    out.reserve(units.len());
    for c in char::decode_utf16(units.iter().copied()) {
        let c = c.unwrap_or('\u{FFFD}');
        let mut b = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }
}

/// Decodes a whole buffer (for previews and tests).
pub fn decode_all(data: &[u8]) -> (Encoding, String) {
    let (enc, bom) = detect(data);
    let mut out = Vec::with_capacity(data.len());
    let mut d = Decoder {
        enc,
        carry: Vec::new(),
    };
    d.decode(&data[bom..], &mut out);
    d.finish(&mut out);
    (enc, String::from_utf8_lossy(&out).into_owned())
}

mod ansi {
    use std::sync::OnceLock;
    use windows_sys::Win32::Globalization::{
        CP_ACP, CPINFO, GetCPInfo, IsDBCSLeadByteEx, MultiByteToWideChar,
    };

    /// The ANSI code page has double-byte characters.
    pub fn dbcs() -> bool {
        static D: OnceLock<bool> = OnceLock::new();
        *D.get_or_init(|| {
            let mut info: CPINFO = unsafe { std::mem::zeroed() };
            unsafe { GetCPInfo(CP_ACP, &mut info) != 0 && info.MaxCharSize > 1 }
        })
    }

    pub fn lead(b: u8) -> bool {
        unsafe { IsDBCSLeadByteEx(CP_ACP, b) != 0 }
    }

    pub fn to_wide(data: &[u8]) -> Vec<u16> {
        let mut out = Vec::new();
        for part in data.chunks(1 << 30) {
            let n = unsafe {
                MultiByteToWideChar(
                    CP_ACP,
                    0,
                    part.as_ptr(),
                    part.len() as i32,
                    std::ptr::null_mut(),
                    0,
                )
            };
            let at = out.len();
            out.resize(at + n.max(0) as usize, 0);
            unsafe {
                MultiByteToWideChar(
                    CP_ACP,
                    0,
                    part.as_ptr(),
                    part.len() as i32,
                    out[at..].as_mut_ptr(),
                    n,
                )
            };
        }
        out
    }
}

/// Bytes of `data` that end on a whole character of the ANSI code page.
fn ansi_complete(data: &[u8]) -> usize {
    if !ansi::dbcs() {
        return data.len();
    }
    let mut i = 0;
    while i < data.len() {
        if ansi::lead(data[i]) {
            if i + 1 >= data.len() {
                return i;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    data.len()
}

fn ansi_to_utf8(data: &[u8], out: &mut Vec<u8>) {
    if data.is_ascii() {
        out.extend_from_slice(data);
    } else {
        push_utf16(&ansi::to_wide(data), out);
    }
}

// ---- matching files ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ContentOptions {
    /// Larger files are skipped.
    pub max_size: u64,
    /// Search files that look binary too (as raw bytes).
    pub binary: bool,
    /// Extract the text of documents with their IFilter.
    pub ifilter: bool,
    /// Stop (the rest unchecked) after reading this many bytes in total.
    pub byte_budget: Option<u64>,
    /// Stop (the rest unchecked) after this long.
    pub time_limit: Option<Duration>,
}

impl Default for ContentOptions {
    fn default() -> Self {
        ContentOptions {
            max_size: 64 << 20,
            binary: false,
            ifilter: true,
            byte_budget: None,
            time_limit: None,
        }
    }
}

/// What reading one file decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Match,
    NoMatch,
    Folder,
    TooLarge,
    Binary,
    /// An online-only cloud file (reading it would download it).
    Offline,
    /// Couldn't open or read it (access denied, gone, locked).
    Unreadable,
    /// Cancelled or out of byte budget before it was read.
    Unchecked,
}

impl Outcome {
    /// Whether the row stays in the results (matches, and rows not looked at yet).
    pub fn keeps(self) -> bool {
        matches!(self, Outcome::Match | Outcome::Unchecked)
    }
}

pub const CHUNK: usize = 1 << 20;
const OVERLAP: usize = 64 << 10;

/// A shared limit on bytes read.
pub struct Budget {
    limit: u64,
    used: AtomicU64,
    /// A read was refused: some file wasn't (fully) read.
    refused: AtomicBool,
}

impl Budget {
    pub fn new(limit: Option<u64>) -> Budget {
        Budget {
            limit: limit.unwrap_or(u64::MAX),
            used: AtomicU64::new(0),
            refused: AtomicBool::new(false),
        }
    }
    pub(crate) fn take(&self, n: u64) -> bool {
        let before = self.used.fetch_add(n, Relaxed);
        if before.saturating_add(n) > self.limit {
            self.used.fetch_sub(n, Relaxed);
            self.refused.store(true, Relaxed);
            false
        } else {
            true
        }
    }
    fn give_back(&self, n: u64) {
        self.used.fetch_sub(n, Relaxed);
    }
    pub fn used(&self) -> u64 {
        self.used.load(Relaxed)
    }
    /// Out of budget: full, or it already had to refuse a read (the last
    /// few bytes rarely fit a whole chunk).
    pub fn exhausted(&self) -> bool {
        self.limit != u64::MAX && (self.used() >= self.limit || self.refused.load(Relaxed))
    }
}

/// File attributes of cloud placeholders whose data isn't on the disk.
const OFFLINE_ATTRS: u32 = 0x1000 | 0x4_0000 | 0x40_0000;

/// Extensions read through an IFilter (when one is registered).
const IFILTER_EXTS: &[&str] = &[
    "doc", "docm", "docx", "dot", "dotx", "odp", "ods", "odt", "pdf", "ppt", "pptm", "pptx", "rtf",
    "xls", "xlsb", "xlsm", "xlsx", "msg", "one", "epub", "vsdx",
];

pub fn uses_ifilter(name: &str) -> bool {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    IFILTER_EXTS.contains(&ext.as_str())
}

impl ContentQuery {
    fn has_neg(&self) -> bool {
        self.terms.iter().any(|t| t.neg)
    }

    /// Checks what is read from `r` (`len` bytes, for the budget).
    /// `chunk` is the read size ([`CHUNK`]; smaller in tests).
    pub fn check_reader(
        &self,
        r: &mut impl Read,
        len: u64,
        opts: &ContentOptions,
        budget: &Budget,
        cancel: &AtomicBool,
        chunk: usize,
    ) -> Outcome {
        let mut found = vec![false; self.terms.len()];
        let need_all = self.has_neg();
        let mut raw = vec![0u8; chunk.max(4)];
        let mut text: Vec<u8> = Vec::with_capacity(chunk + OVERLAP);
        let mut dec: Option<Decoder> = None;
        let mut left = len;
        loop {
            if cancel.load(Relaxed) {
                return Outcome::Unchecked;
            }
            // The first read is enough to see a BOM and tell binary files (most
            // of them) apart, without reading a whole chunk of each.
            let first = if dec.is_none() {
                chunk.clamp(4, 64 << 10)
            } else {
                chunk
            };
            let want = (first as u64).min(left.max(1));
            if !budget.take(want) {
                return Outcome::Unchecked;
            }
            let n = match read_full(r, &mut raw[..want as usize]) {
                Ok(n) => n,
                Err(_) => {
                    budget.give_back(want);
                    return Outcome::Unreadable;
                }
            };
            budget.give_back(want - n as u64);
            left = left.saturating_sub(n as u64);
            let mut data = &raw[..n];
            if dec.is_none() {
                let (enc, bom) = detect(data);
                if enc == Encoding::Binary && !opts.binary {
                    return Outcome::Binary;
                }
                data = &data[bom..];
                dec = Some(Decoder {
                    enc,
                    carry: Vec::new(),
                });
            }
            let d = dec.as_mut().unwrap();
            // Keep the end of the previous text so matches across the boundary are found.
            let mut keep = text.len().saturating_sub(OVERLAP);
            while keep < text.len() && text[keep] & 0xC0 == 0x80 {
                keep += 1;
            }
            text.drain(..keep);
            let eof = n < want as usize || left == 0;
            d.decode(data, &mut text);
            if eof {
                d.finish(&mut text);
            }
            for (t, f) in self.terms.iter().zip(found.iter_mut()) {
                if !*f && t.re.is_match(&text) {
                    if t.neg {
                        return Outcome::NoMatch;
                    }
                    *f = true;
                }
            }
            if !need_all && found.iter().all(|&f| f) {
                return Outcome::Match;
            }
            if eof {
                break;
            }
        }
        let ok = self.terms.iter().zip(&found).all(|(t, &f)| t.neg || f);
        if ok { Outcome::Match } else { Outcome::NoMatch }
    }

    /// Checks one in-memory file (tests, previews).
    pub fn check_bytes(&self, data: &[u8], opts: &ContentOptions) -> Outcome {
        self.check_reader(
            &mut &data[..],
            data.len() as u64,
            opts,
            &Budget::new(None),
            &AtomicBool::new(false),
            CHUNK,
        )
    }

    /// Reads one result row's file and decides.
    pub fn check_row(
        &self,
        row: &Row,
        opts: &ContentOptions,
        budget: &Budget,
        cancel: &AtomicBool,
    ) -> Outcome {
        if row.is_dir {
            return Outcome::Folder;
        }
        if cancel.load(Relaxed) || budget.exhausted() {
            return Outcome::Unchecked;
        }
        if row.attrs & OFFLINE_ATTRS != 0 {
            return Outcome::Offline;
        }
        if row.has_size && row.size > opts.max_size {
            return Outcome::TooLarge;
        }
        self.check_path(&row.full_path(), opts, budget, cancel)
    }

    pub fn check_path(
        &self,
        path: &str,
        opts: &ContentOptions,
        budget: &Budget,
        cancel: &AtomicBool,
    ) -> Outcome {
        if opts.ifilter && uses_ifilter(path) {
            match crate::ifilter::extract(path, opts.max_size, budget, cancel) {
                crate::ifilter::Extracted::Text(t) => {
                    let utf8 = ContentOptions {
                        binary: true,
                        ..opts.clone()
                    };
                    let free = Budget::new(None); // already counted while extracting
                    return self.check_reader(
                        &mut t.as_bytes(),
                        t.len() as u64,
                        &utf8,
                        &free,
                        cancel,
                        CHUNK,
                    );
                }
                crate::ifilter::Extracted::Unchecked => return Outcome::Unchecked,
                crate::ifilter::Extracted::Failed => return Outcome::Unreadable,
                crate::ifilter::Extracted::NoFilter => {} // read it raw (and probably find it binary)
            }
        }
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        let Ok(mut f) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path)
        else {
            return Outcome::Unreadable;
        };
        let len = match f.metadata() {
            Ok(m) if m.is_dir() => return Outcome::Folder,
            Ok(m) => m.len(),
            Err(_) => return Outcome::Unreadable,
        };
        if len > opts.max_size {
            return Outcome::TooLarge;
        }
        self.check_reader(&mut f, len, opts, budget, cancel, CHUNK)
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

// ---- scanning a result set -------------------------------------------------------

/// Threads reading files: more than cores, since most of the time is I/O.
fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map_or(8, |n| n.get())
            .clamp(8, 32);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("content-{i}"))
            .build()
            .unwrap()
    })
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// Rows the name search found (what is being checked).
    pub total: u64,
    pub checked: u64,
    pub matched: u64,
    pub folders: u64,
    pub too_large: u64,
    pub binary: u64,
    pub offline: u64,
    pub unreadable: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
    pub budget_exhausted: bool,
    /// Stopped at [`ContentOptions::time_limit`].
    pub timed_out: bool,
    pub done: bool,
}

impl Stats {
    fn add(&mut self, o: Outcome) {
        if o != Outcome::Unchecked {
            self.checked += 1;
        }
        match o {
            Outcome::Match => self.matched += 1,
            Outcome::Folder => self.folders += 1,
            Outcome::TooLarge => self.too_large += 1,
            Outcome::Binary => self.binary += 1,
            Outcome::Offline => self.offline += 1,
            Outcome::Unreadable => self.unreadable += 1,
            Outcome::NoMatch | Outcome::Unchecked => {}
        }
    }

    /// Files that were skipped rather than searched, as a phrase ("" if none).
    pub fn skipped(&self) -> String {
        let mut v = Vec::new();
        for (n, what) in [
            (self.too_large, "too large"),
            (self.binary, "binary"),
            (self.offline, "online-only"),
            (self.unreadable, "unreadable"),
        ] {
            if n > 0 {
                v.push(format!("{n} {what}"));
            }
        }
        v.join(", ")
    }
}

pub struct Batch {
    /// Snapshot position of `rows[0]`.
    pub offset: u64,
    pub rows: Vec<Row>,
    pub outcomes: Vec<Outcome>,
}

impl Batch {
    /// Snapshot positions of rows that failed.
    pub fn rejected(&self) -> impl Iterator<Item = u32> + '_ {
        self.outcomes
            .iter()
            .enumerate()
            .filter(|(_, o)| !o.keeps())
            .map(|(i, _)| (self.offset + i as u64) as u32)
    }
}

/// Walks the session's current result set in display order and checks
/// each row's contents.
pub struct Scan {
    pub q: Arc<ContentQuery>,
    pub opts: ContentOptions,
    pub stats: Stats,
    pub cancel: Arc<AtomicBool>,
    budget: Budget,
    next: u64,
    batch: u32,
    start: Instant,
}

impl Scan {
    pub fn new(q: Arc<ContentQuery>, opts: ContentOptions, cancel: Arc<AtomicBool>) -> Scan {
        let budget = Budget::new(opts.byte_budget);
        Scan {
            q,
            opts,
            stats: Stats::default(),
            cancel,
            budget,
            next: 0,
            batch: 128,
            start: Instant::now(),
        }
    }

    /// Fetches and checks the next rows; `None` once everything is checked
    /// (or the scan was cancelled or ran out of budget: see [`Scan::stats`]).
    pub fn next_batch(
        &mut self,
        call: &mut dyn FnMut(&Request) -> io::Result<Response>,
    ) -> Result<Option<Batch>, String> {
        if self.stats.done {
            return Ok(None);
        }
        if self.cancel.load(Relaxed) || self.budget.exhausted() {
            self.finish();
            return Ok(None);
        }
        if self
            .opts
            .time_limit
            .is_some_and(|t| self.start.elapsed() >= t)
        {
            self.stats.timed_out = true;
            self.finish();
            return Ok(None);
        }
        let page = match call(&Request::Snapshot {
            offset: self.next,
            count: self.batch,
        }) {
            Ok(Response::Page(p)) => p,
            Ok(Response::Error(e)) if e.contains("request kind") => {
                return Err("the rapidfind service is older than this program; content search needs `rapidfind service update` (elevated)".into());
            }
            Ok(Response::Error(e)) => return Err(e),
            Ok(_) => return Err("unexpected response".into()),
            Err(e) => return Err(format!("service connection: {e}")),
        };
        self.stats.total = page.total;
        if page.rows.is_empty() {
            self.finish();
            return Ok(None);
        }
        let (q, opts, budget, cancel) = (&*self.q, &self.opts, &self.budget, &*self.cancel);
        let outcomes: Vec<Outcome> = pool().install(|| {
            page.rows
                .par_iter()
                .with_max_len(1)
                .map(|r| q.check_row(r, opts, budget, cancel))
                .collect()
        });
        for &o in &outcomes {
            self.stats.add(o);
        }
        self.stats.bytes = self.budget.used();
        self.stats.elapsed = self.start.elapsed();
        let b = Batch {
            offset: self.next,
            rows: page.rows,
            outcomes,
        };
        self.next += b.rows.len() as u64;
        self.batch = (self.batch * 2).min(1024);
        if b.outcomes.contains(&Outcome::Unchecked) {
            self.finish();
        }
        Ok(Some(b))
    }

    fn finish(&mut self) {
        self.stats.done = true;
        self.stats.cancelled = self.cancel.load(Relaxed);
        self.stats.budget_exhausted = !self.stats.cancelled && self.budget.exhausted();
        self.stats.bytes = self.budget.used();
        self.stats.elapsed = self.start.elapsed();
    }
}

/// Runs a whole scan over the session's result set and returns the matching
/// rows in order, stopping early once `want` of them were found.
pub fn matching_rows(
    call: &mut dyn FnMut(&Request) -> io::Result<Response>,
    q: Arc<ContentQuery>,
    opts: ContentOptions,
    want: Option<usize>,
) -> Result<(Vec<Row>, Stats), String> {
    let mut scan = Scan::new(q, opts, Arc::new(AtomicBool::new(false)));
    let mut rows = Vec::new();
    while let Some(b) = scan.next_batch(call)? {
        rows.extend(
            b.rows
                .into_iter()
                .zip(b.outcomes)
                .filter(|(_, o)| *o == Outcome::Match)
                .map(|(r, _)| r),
        );
        if want.is_some_and(|w| rows.len() >= w) {
            scan.stats.elapsed = scan.start.elapsed();
            break;
        }
    }
    Ok((rows, scan.stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> ContentQuery {
        split_query(s, SearchFlags::default()).unwrap().1.unwrap()
    }

    fn utf16(s: &str, be: bool, bom: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if bom {
            v.extend_from_slice(if be { &[0xFE, 0xFF] } else { &[0xFF, 0xFE] });
        }
        for u in s.encode_utf16() {
            v.extend_from_slice(&if be { u.to_be_bytes() } else { u.to_le_bytes() });
        }
        v
    }

    fn check(query: &str, data: &[u8]) -> Outcome {
        q(query).check_bytes(data, &ContentOptions::default())
    }

    #[test]
    fn splits_content_terms_off() {
        let f = SearchFlags::default();
        let (rest, c) = split_query(r#"*.rs content:"fn main" !content:test ext:txt"#, f).unwrap();
        assert_eq!(rest, "*.rs ext:txt");
        let c = c.unwrap();
        assert_eq!(
            (c.terms[0].text.as_str(), c.terms[0].neg),
            ("fn main", false)
        );
        assert_eq!((c.terms[1].text.as_str(), c.terms[1].neg), ("test", true));
        assert_eq!(c.describe(), r#""fn main" and not "test""#);
        let (rest, none) = split_query("foo bar", f).unwrap();
        assert!(rest == "foo bar" && none.is_none());
        // Modifiers before content:, the Search menu's flags as defaults.
        let c = split_query("case:content:Foo", f).unwrap().1.unwrap();
        assert!(c.terms[0].case && !c.terms[0].regex);
        let c = split_query("regex:content:fo+ content-regex:^x", f)
            .unwrap()
            .1
            .unwrap();
        assert!(c.terms.iter().all(|t| t.regex));
        let c = split_query(
            "content:foo",
            SearchFlags {
                case: true,
                whole_word: true,
                ..f
            },
        )
        .unwrap()
        .1
        .unwrap();
        assert!(c.terms[0].case && c.terms[0].whole_word);
        assert!(
            !split_query("nocase:content:foo", SearchFlags { case: true, ..f })
                .unwrap()
                .1
                .unwrap()
                .terms[0]
                .case
        );
        // Inside plain AND groups is fine (the MCP server wraps queries in `< >`).
        assert_eq!(split_query("<*.txt content:x >", f).unwrap().0, "<*.txt >");
        // ... but not under OR or NOT.
        for bad in [
            "content:a | *.txt",
            "*.txt|content:a",
            "<content:a b>|c",
            "!<content:a>",
            "content:",
            "regex:content:(",
        ] {
            assert!(split_query(bad, f).is_err(), "{bad}");
        }
        // Not content terms: a plain name with a colon-free word, regex on names.
        assert!(split_query("regex:content:foo", f).unwrap().1.is_some());
        assert_eq!(
            split_query("wfn:content", f)
                .unwrap()
                .1
                .map(|c| c.terms.len()),
            None
        );
        // The service refuses what slipped through.
        assert!(crate::query::Query::parse("content:x").is_err());
    }

    #[test]
    fn matches_encodings() {
        let text = "first line\nThe Quick brown fox — ünïcødé\n";
        let utf8 = text.as_bytes().to_vec();
        let mut bom8 = vec![0xEF, 0xBB, 0xBF];
        bom8.extend_from_slice(text.as_bytes());
        for (name, data) in [
            ("utf-8", utf8),
            ("utf-8 bom", bom8),
            ("utf-16le bom", utf16(text, false, true)),
            ("utf-16be bom", utf16(text, true, true)),
            ("utf-16le no bom", utf16(text, false, false)),
        ] {
            assert_eq!(check("content:quick", &data), Outcome::Match, "{name}");
            assert_eq!(
                check("content:ÜNÏCØDÉ", &data),
                Outcome::Match,
                "{name}: case folding beyond ASCII"
            );
            assert_eq!(
                check("case:content:quick", &data),
                Outcome::NoMatch,
                "{name}"
            );
            assert_eq!(
                check("content:\"brown fox\"", &data),
                Outcome::Match,
                "{name}"
            );
            assert_eq!(check("content:missing", &data), Outcome::NoMatch, "{name}");
            assert_eq!(check("!content:missing", &data), Outcome::Match, "{name}");
            assert_eq!(
                check("content:quick !content:fox", &data),
                Outcome::NoMatch,
                "{name}"
            );
            assert_eq!(
                check("regex:content:^the\\s+q", &data),
                Outcome::Match,
                "{name}: ^ is per line"
            );
            assert_eq!(check("ww:content:quic", &data), Outcome::NoMatch, "{name}");
            assert_eq!(check("ww:content:quick", &data), Outcome::Match, "{name}");
        }
        assert_eq!(detect(&utf16("abc", false, false)).0, Encoding::Utf16Le);
        assert_eq!(detect(&utf16("abc", true, false)).0, Encoding::Utf16Be);
        assert_eq!(
            decode_all(&utf16("håj 😀", false, true)),
            (Encoding::Utf16Le, "håj 😀".into())
        );
    }

    #[test]
    fn binary_files_are_skipped_unless_asked() {
        let mut data = b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\xff\xff needle \x00\x01\x02".to_vec();
        data.extend(std::iter::repeat_n(0xAB, 100));
        assert_eq!(detect(&data).0, Encoding::Binary);
        assert_eq!(check("content:needle", &data), Outcome::Binary);
        let opts = ContentOptions {
            binary: true,
            ..Default::default()
        };
        assert_eq!(
            q("content:needle").check_bytes(&data, &opts),
            Outcome::Match
        );
        assert_eq!(check("content:x", b""), Outcome::NoMatch);
        assert_eq!(check("!content:x", b""), Outcome::Match);
    }

    #[test]
    fn ansi_text_is_decoded() {
        // "café" in Windows-1252 isn't valid UTF-8.
        let data = b"caf\xe9 au lait";
        assert_eq!(detect(data).0, Encoding::Ansi);
        if unsafe { windows_sys::Win32::Globalization::GetACP() } == 1252 {
            assert_eq!(check("content:café", data), Outcome::Match);
        }
        assert_eq!(check("content:lait", data), Outcome::Match);
    }

    #[test]
    fn matches_across_chunk_boundaries() {
        let cq = q("content:needle");
        let opts = ContentOptions::default();
        let none = AtomicBool::new(false);
        for enc in ["utf8", "utf16"] {
            for pad in 0..40 {
                let text = format!("{}needle{}", "x".repeat(pad), "y".repeat(50));
                let data = if enc == "utf8" {
                    text.clone().into_bytes()
                } else {
                    utf16(&text, false, true)
                };
                // 7-byte chunks split the needle, UTF-16 units and the BOM.
                let got = cq.check_reader(
                    &mut &data[..],
                    data.len() as u64,
                    &opts,
                    &Budget::new(None),
                    &none,
                    7,
                );
                assert_eq!(got, Outcome::Match, "{enc} pad {pad}");
            }
        }
        // A surrogate pair split across chunks survives.
        let data = utf16("a😀needle", false, true);
        let cq = q("content:😀needle");
        for chunk in 1..12 {
            assert_eq!(
                cq.check_reader(
                    &mut &data[..],
                    data.len() as u64,
                    &opts,
                    &Budget::new(None),
                    &none,
                    chunk
                ),
                Outcome::Match,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn budget_counts_as_exhausted_once_a_read_is_refused() {
        let b = Budget::new(Some(10));
        assert!(b.take(8) && !b.exhausted());
        assert!(!b.take(5), "over the limit");
        assert!(b.exhausted(), "2 bytes left, but a file went unread");
        assert!(!Budget::new(None).exhausted());
    }

    #[test]
    fn budget_and_cancel_leave_files_unchecked() {
        let cq = q("content:zzz");
        let data = vec![b'a'; 10_000];
        let none = AtomicBool::new(false);
        let opts = ContentOptions::default();
        let b = Budget::new(Some(4_000));
        assert_eq!(
            cq.check_reader(&mut &data[..], 10_000, &opts, &b, &none, 1000),
            Outcome::Unchecked
        );
        assert!(b.used() <= 4_000);
        let b = Budget::new(Some(20_000));
        assert_eq!(
            cq.check_reader(&mut &data[..], 10_000, &opts, &b, &none, 1000),
            Outcome::NoMatch
        );
        assert_eq!(b.used(), 10_000);
        let stop = AtomicBool::new(true);
        assert_eq!(
            cq.check_reader(
                &mut &data[..],
                10_000,
                &opts,
                &Budget::new(None),
                &stop,
                1000
            ),
            Outcome::Unchecked
        );
    }

    /// The whole flow against a session: snapshot in display order, drop
    /// the rejects, stop early on cancel with the rest kept.
    #[test]
    fn scan_drops_rejects_and_stops_on_cancel() {
        use crate::ipc::{Backend, Local};
        let dir =
            std::env::temp_dir().join(format!("rapidfind-content-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..600 {
            std::fs::write(
                dir.join(format!("f{i:03}.txt")),
                if i % 100 == 7 {
                    "has the needle"
                } else {
                    "plain"
                },
            )
            .unwrap();
        }
        let ix = crate::folder::build(&dir.to_string_lossy(), crate::fold::FoldMode::Lower);
        let mut be = Local::new(Arc::new(crate::engine::Engine::from_indexes(vec![ix])));
        let cq = Arc::new(q("content:needle"));
        let f = SearchFlags::default();
        assert_eq!(
            be.search("*.txt", f, crate::search::SortKey::Name, false, 10)
                .unwrap()
                .total,
            600
        );
        let mut scan = Scan::new(
            cq.clone(),
            ContentOptions::default(),
            Arc::new(AtomicBool::new(false)),
        );
        let mut rejected = Vec::new();
        while let Some(b) = scan.next_batch(&mut |r| be.call(r)).unwrap() {
            rejected.extend(b.rejected());
        }
        assert!(scan.stats.done && !scan.stats.cancelled);
        assert_eq!((scan.stats.checked, scan.stats.matched), (600, 6));
        let page = be.drop_rows(rejected, 100).unwrap();
        let names: Vec<_> = page.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "f007.txt", "f107.txt", "f207.txt", "f307.txt", "f407.txt", "f507.txt"
            ]
        );

        // Cancelled after the first batch: what wasn't read stays.
        be.search("*.txt", f, crate::search::SortKey::Name, false, 10)
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut scan = Scan::new(cq, ContentOptions::default(), cancel.clone());
        let first = scan.next_batch(&mut |r| be.call(r)).unwrap().unwrap();
        let read = first.rows.len();
        let hits = first
            .outcomes
            .iter()
            .filter(|o| **o == Outcome::Match)
            .count();
        let mut rejected: Vec<u32> = first.rejected().collect();
        cancel.store(true, Relaxed);
        while let Some(b) = scan.next_batch(&mut |r| be.call(r)).unwrap() {
            rejected.extend(b.rejected());
        }
        assert!(scan.stats.cancelled && scan.stats.done);
        assert_eq!(scan.stats.checked as usize, read);
        let page = be.drop_rows(rejected, 1000).unwrap();
        assert!(read < 600 && hits > 0);
        assert_eq!(
            page.total as usize,
            600 - read + hits,
            "unread rows are kept, plus the matches read so far"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highlight_ranges() {
        let c = q("content:ab !content:zz content:cd");
        assert_eq!(c.find_ranges(b"xxABcdab", 10), vec![2..4, 4..6, 6..8]);
        assert_eq!(c.find_ranges(b"abab", 1), vec![0..2]);
    }
}
