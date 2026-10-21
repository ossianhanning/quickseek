//! Query language: a practical subset of Everything's search syntax.
//!
//! | syntax                    | meaning                                                  |
//! |---------------------------|----------------------------------------------------------|
//! | `foo bar`                 | name contains `foo` AND `bar` (case-insensitive)         |
//! | `foo\|bar`                | OR (binds tighter than AND, Everything's default)        |
//! | `!foo`, `!<a b>`          | NOT                                                      |
//! | `<a b>\|c`                | grouping                                                 |
//! | `"a b"`                   | literal containing spaces                                |
//! | `*.rs`, `a?c`             | wildcards match the whole name                           |
//! | `users\me`, `users/me`    | a path separator matches against the full path           |
//! | `users\*\*.pdf`           | in a path, `*` spans any number of folders; the pattern may start anywhere but must match to the end |
//! | `case:` `ww:` `path:` `regex:` (and `nocase:` `nopath:` ...) | per-term options   |
//! | `wfn:node_modules`        | the whole name (or path) exactly                         |
//! | `ext:jpg;png`             | extension list                                           |
//! | `size:>10mb`, `size:1mb..2mb` | size compare                                         |
//! | `dm:` `dc:` `rc:`         | date modified / created / recently changed (see [`crate::date`]) |
//! | `parent:C:\dir`           | direct children of a folder                              |
//! | `depth:2`, `depth:>3`     | number of path components below the volume root          |
//! | `empty:`                  | folders without children                                 |
//! | `attrib:HS`               | attributes (R H S D A C E O L T P I)                     |
//! | `runcount:>0`             | opened from the GUI at least once                        |
//! | `dupe:` / `sizedupe:`     | only names (or sizes) occurring more than once           |
//! | `file:` `folder:`         | only files / only folders                                |
//! | `audio:` `video:` `pic:` `doc:` `exe:` `zip:` | extension groups (Everything's filters) |

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::date::{DateEnv, parse_date_filter};
use crate::fold::{FoldMode, fold_mode};
use crate::index::Index;

pub const AUDIO: &str = "aac;ac3;aif;aifc;aiff;amr;ape;au;cda;dts;fla;flac;it;m1a;m2a;m3u;m4a;m4b;m4p;mid;midi;mka;mod;mp2;mp3;mpa;ogg;opus;ra;rmi;snd;spc;umx;voc;wav;weba;wma;xm";
pub const COMPRESSED: &str = "7z;ace;arj;bz2;cab;gz;gzip;jar;lz;lz4;lzma;r00;r01;r02;r03;r04;r05;r06;r07;r08;r09;r10;r11;r12;r13;r14;r15;r16;r17;r18;r19;r20;r21;r22;r23;r24;r25;r26;r27;r28;r29;rar;tar;tbz;tbz2;tgz;txz;xz;z;zip;zipx;zst";
pub const DOCUMENT: &str = "c;chm;cpp;csv;cxx;doc;docm;docx;dot;dotm;dotx;epub;h;hpp;htm;html;hxx;ini;java;json;log;lua;md;mht;mhtml;odp;ods;odt;pdf;potm;potx;ppam;pps;ppsm;ppsx;ppt;pptm;pptx;rs;rtf;sldm;sldx;thmx;toml;txt;vsd;wpd;wps;wri;xlam;xls;xlsb;xlsm;xlsx;xltm;xltx;xml;yaml;yml";
pub const EXECUTABLE: &str = "bat;cmd;com;exe;msi;msix;msp;ps1;scr";
pub const PICTURE: &str = "ani;avif;bmp;dds;exr;gif;hdr;heic;heif;ico;jfif;jpe;jpeg;jpg;jxl;pcx;png;psd;raw;svg;tga;tif;tiff;webp;wmf";
pub const VIDEO: &str = "3g2;3gp;3gp2;3gpp;amv;asf;avi;bik;divx;drc;dsa;dsm;dss;dsv;evo;f4v;flc;fli;flic;flv;hdmov;ifo;ivf;m1v;m2p;m2t;m2ts;m2v;m4v;mkv;mov;mp2v;mp4;mp4v;mpe;mpeg;mpg;mpls;mpv2;mpv4;mts;ogm;ogv;pss;pva;qt;ram;ratdvd;rm;rmm;rmvb;roq;rpm;smil;smk;swf;tp;tpr;ts;vob;vp6;webm;wm;wmp;wmv";

/// Everything's filter dropdown, as query macros.
pub const FILTERS: &[(&str, &str)] = &[
    ("Everything", ""),
    ("Audio", "audio:"),
    ("Compressed", "zip:"),
    ("Document", "doc:"),
    ("Executable", "exe:"),
    ("Folder", "folder:"),
    ("Picture", "pic:"),
    ("Video", "video:"),
];

#[derive(Debug)]
pub enum Node {
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateField {
    Modified,
    Created,
    RecentChange,
}

#[derive(Debug)]
pub enum Leaf {
    /// Substring. `needle` is folded unless `case`.
    Text {
        needle: Vec<u8>,
        case: bool,
        path: bool,
        whole_word: bool,
    },
    /// Whole-name (or whole-path) wildcard pattern. Path patterns are also
    /// in [`Query::path_globs`] at `slot`.
    Glob {
        pat: Vec<u8>,
        case: bool,
        path: bool,
        slot: usize,
    },
    Regex {
        re: regex::bytes::Regex,
        path: bool,
    },
    /// Folded extensions without the dot.
    Ext(HashSet<Vec<u8>>),
    Size {
        lo: u64,
        hi: u64,
    },
    IsFile,
    IsDir,
    /// Half-open UTC FILETIME interval.
    Date {
        field: DateField,
        lo: i64,
        hi: i64,
    },
    /// Index into [`Query::parents`].
    Parent(usize),
    Depth {
        lo: u64,
        hi: u64,
    },
    Empty,
    Attrib(u32),
    RunCount {
        lo: u64,
        hi: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dupe {
    Name,
    Size,
}

/// Search-wide defaults (the GUI's Search menu toggles).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchFlags {
    pub case: bool,
    pub whole_word: bool,
    pub path: bool,
    pub regex: bool,
}

impl SearchFlags {
    pub fn bits(self) -> u8 {
        self.case as u8
            | (self.whole_word as u8) << 1
            | (self.path as u8) << 2
            | (self.regex as u8) << 3
    }
    pub fn from_bits(b: u8) -> Self {
        SearchFlags {
            case: b & 1 != 0,
            whole_word: b & 2 != 0,
            path: b & 4 != 0,
            regex: b & 8 != 0,
        }
    }
}

#[derive(Debug)]
pub struct Query {
    pub root: Node,
    pub dupe: Option<Dupe>,
    /// Folded folder paths referenced by `parent:` (no trailing `\`).
    pub parents: Vec<Vec<u8>>,
    pub fold: FoldMode,
    pub uses_child_counts: bool,
    pub uses_runs: bool,
    pub uses_recent: bool,
    pub uses_created: bool,
    /// Path wildcard patterns (folded unless case-sensitive) and their case flag.
    pub path_globs: Vec<(Vec<u8>, bool)>,
}

#[derive(Default, Clone, Copy)]
struct Mods {
    case: bool,
    path: bool,
    ww: bool,
    regex: bool,
    /// The term is the whole name (or path), not a substring.
    exact: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Tok {
    Word(String),
    Or,
    Not,
    Open,
    Close,
}

fn tokenize(s: &str) -> Vec<Tok> {
    tokenize_spans(s).into_iter().map(|(t, _)| t).collect()
}

/// Tokens with the byte range each one came from (for operators, the
/// operator character itself).
pub(crate) fn tokenize_spans(s: &str) -> Vec<(Tok, std::ops::Range<usize>)> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0usize;
    let mut quoted = false;
    let mut depth = 0usize;
    let flush = |cur: &mut String, start: usize, out: &mut Vec<(Tok, std::ops::Range<usize>)>| {
        if !cur.is_empty() {
            let end = start + cur.len();
            out.push((Tok::Word(std::mem::take(cur)), start..end));
        }
    };
    for (i, &(at, c)) in chars.iter().enumerate() {
        if cur.is_empty() {
            start = at;
        }
        if c == '"' {
            quoted = !quoted;
            cur.push(c);
            continue;
        }
        if quoted {
            cur.push(c);
            continue;
        }
        match c {
            c if c.is_whitespace() => flush(&mut cur, start, &mut out),
            '|' => {
                flush(&mut cur, start, &mut out);
                out.push((Tok::Or, at..at + 1));
            }
            '<' if cur.chars().all(|c| c == '!') => {
                for k in 0..cur.len() {
                    out.push((Tok::Not, start + k..start + k + 1));
                }
                cur.clear();
                out.push((Tok::Open, at..at + 1));
                depth += 1;
            }
            '>' if depth > 0
                && !cur.ends_with([':', '<', '>', '='])
                && chars
                    .get(i + 1)
                    .is_none_or(|(_, n)| n.is_whitespace() || *n == '|' || *n == '>') =>
            {
                flush(&mut cur, start, &mut out);
                out.push((Tok::Close, at..at + 1));
                depth -= 1;
            }
            c => cur.push(c),
        }
    }
    flush(&mut cur, start, &mut out);
    out
}

fn parse_num(s: &str, units: bool) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = if units {
        [
            ("tb", 1u64 << 40),
            ("gb", 1 << 30),
            ("mb", 1 << 20),
            ("kb", 1 << 10),
            ("b", 1),
        ]
        .iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n.to_string(), *m)))
        .unwrap_or((s.clone(), 1))
    } else {
        (s.clone(), 1)
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mul as f64) as u64)
}

/// `a..b`, `<x`, `<=x`, `>x`, `>=x`, `=x`, `x` → inclusive range.
fn parse_range(v: &str, units: bool) -> Option<(u64, u64)> {
    Some(if let Some((a, b)) = v.split_once("..") {
        (parse_num(a, units)?, parse_num(b, units)?)
    } else if let Some(x) = v.strip_prefix(">=") {
        (parse_num(x, units)?, u64::MAX)
    } else if let Some(x) = v.strip_prefix("<=") {
        (0, parse_num(x, units)?)
    } else if let Some(x) = v.strip_prefix('>') {
        (parse_num(x, units)?.saturating_add(1), u64::MAX)
    } else if let Some(x) = v.strip_prefix('<') {
        (0, parse_num(x, units)?.checked_sub(1)?)
    } else {
        let x = parse_num(v.strip_prefix('=').unwrap_or(v), units)?;
        (x, x)
    })
}

fn parse_attrib(v: &str) -> Option<u32> {
    let mut m = 0;
    for c in v.chars() {
        m |= match c.to_ascii_uppercase() {
            'R' => 0x1,
            'H' => 0x2,
            'S' => 0x4,
            'D' => 0x10,
            'A' => 0x20,
            'N' => 0x80,
            'T' => 0x100,
            'P' => 0x200,
            'L' => 0x400,
            'C' => 0x800,
            'O' => 0x1000,
            'I' => 0x2000,
            'E' => 0x4000,
            _ => return None,
        };
    }
    Some(m)
}

fn unquote(s: &str) -> String {
    s.replace('"', "")
}

/// A wildcard pattern matched against full paths: `*` (or `**`) spans any
/// number of folders, and the pattern may start anywhere in the path (like a
/// plain path term) but must match through to its end, so `users\*\*.pdf`
/// finds `C:\Users\me\Documents\a.pdf`.
fn path_glob(pat: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(pat.len() + 1);
    if pat.first() != Some(&b'*') {
        out.push(b'*');
    }
    for b in pat {
        if !(b == b'*' && out.last() == Some(&b'*')) {
            out.push(b);
        }
    }
    // `\*\` is "any folders in between", including none: `a\*\b` matches `a\b`.
    let mut i = 0;
    while i + 2 < out.len() {
        if out[i] == b'\\' && out[i + 1] == b'*' && out[i + 2] == b'\\' {
            let mut j = i + 1;
            while out.get(j) == Some(&b'*') && out.get(j + 1) == Some(&b'\\') {
                j += 2; // `\*\*\` is the same as `\*\`
            }
            out.splice(i + 1..j, [ANY_DIRS]);
        }
        i += 1;
    }
    out
}

/// Pattern byte for zero or more whole folders (what `\*\` becomes in a
/// path pattern): matches the empty string or anything ending with `\`.
const ANY_DIRS: u8 = 0x01;

fn is_wild(b: u8) -> bool {
    matches!(b, b'*' | b'?' | ANY_DIRS)
}

/// The literal a wildcard pattern ends with, when it lies within the last
/// path component (so every match's *name* contains it).
fn glob_tail(pat: &[u8]) -> Option<&[u8]> {
    let start = pat.iter().rposition(|&b| is_wild(b)).map_or(0, |p| p + 1);
    let tail = &pat[start..];
    (!tail.is_empty() && !tail.contains(&b'\\')).then_some(tail)
}

fn ext_set(list: &str, fold: FoldMode) -> HashSet<Vec<u8>> {
    list.split(';')
        .map(|e| e.trim().trim_start_matches('.'))
        .filter(|e| !e.is_empty())
        .map(|e| fold_mode(e, fold).into_bytes())
        .collect()
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    defaults: Mods,
    fold: FoldMode,
    env: &'a DateEnv,
    q: Query,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// AND of everything up to a `>` or the end.
    fn parse_and(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            if *t == Tok::Close {
                break;
            }
            if *t == Tok::Or {
                self.pos += 1; // stray `|`
                continue;
            }
            if let Some(n) = self.parse_or()? {
                items.push(n);
            }
        }
        Ok(Node::All(items))
    }

    fn parse_or(&mut self) -> Result<Option<Node>, String> {
        let mut alts = Vec::new();
        if let Some(n) = self.parse_unary()? {
            alts.push(n);
        }
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            if let Some(n) = self.parse_unary()? {
                alts.push(n);
            }
        }
        Ok(match alts.len() {
            0 => None,
            1 => alts.pop(),
            _ => Some(Node::Any(alts)),
        })
    }

    fn parse_unary(&mut self) -> Result<Option<Node>, String> {
        let Some(t) = self.toks.get(self.pos) else {
            return Ok(None);
        };
        match t {
            Tok::Not => {
                self.pos += 1;
                Ok(self.parse_unary()?.map(|n| Node::Not(Box::new(n))))
            }
            Tok::Open => {
                self.pos += 1;
                let n = self.parse_and()?;
                if self.peek() == Some(&Tok::Close) {
                    self.pos += 1;
                }
                Ok(Some(n))
            }
            Tok::Word(w) => {
                let w = w.clone();
                self.pos += 1;
                self.parse_atom(&w)
            }
            Tok::Or | Tok::Close => Ok(None),
        }
    }

    fn parse_atom(&mut self, mut s: &str) -> Result<Option<Node>, String> {
        let mut m = self.defaults;
        let fold = self.fold;
        let mut neg = false;
        while let Some(rest) = s.strip_prefix('!') {
            neg = !neg;
            s = rest;
        }
        let mut filters: Vec<Node> = Vec::new();
        let leaf = |l: Leaf| Node::Leaf(l);
        while let Some((key, rest)) = s.split_once(':') {
            if key.starts_with('"') || key.is_empty() {
                break;
            }
            let value = unquote(rest);
            let mut consumed_value = true;
            match key.to_ascii_lowercase().as_str() {
                "case" => (m.case, consumed_value) = (true, false),
                "nocase" => (m.case, consumed_value) = (false, false),
                "path" => (m.path, consumed_value) = (true, false),
                "nopath" => (m.path, consumed_value) = (false, false),
                "ww" | "wholeword" => (m.ww, consumed_value) = (true, false),
                "noww" | "nowholeword" => (m.ww, consumed_value) = (false, false),
                "regex" => {
                    m.regex = true;
                    s = rest;
                    break; // the rest is the pattern verbatim
                }
                "noregex" => (m.regex, consumed_value) = (false, false),
                k if crate::content::is_content_key(k) => {
                    // Content terms are taken out by the client (crate::content::split_query).
                    return Err(format!(
                        "{k}: searches file contents, which only the result list, `rapidfind search` and the MCP search do, combined with AND"
                    ));
                }
                "wfn" | "wholefilename" | "exact" => (m.exact, consumed_value) = (true, false),
                "file" | "files" => {
                    filters.push(leaf(Leaf::IsFile));
                    consumed_value = false;
                }
                "folder" | "folders" => {
                    filters.push(leaf(Leaf::IsDir));
                    consumed_value = false;
                }
                "audio" => {
                    filters.push(leaf(Leaf::Ext(ext_set(AUDIO, fold))));
                    consumed_value = false;
                }
                "zip" | "compressed" => {
                    filters.push(leaf(Leaf::Ext(ext_set(COMPRESSED, fold))));
                    consumed_value = false;
                }
                "doc" | "document" => {
                    filters.push(leaf(Leaf::Ext(ext_set(DOCUMENT, fold))));
                    consumed_value = false;
                }
                "exe" | "executable" => {
                    filters.push(leaf(Leaf::Ext(ext_set(EXECUTABLE, fold))));
                    consumed_value = false;
                }
                "pic" | "picture" => {
                    filters.push(leaf(Leaf::Ext(ext_set(PICTURE, fold))));
                    consumed_value = false;
                }
                "video" => {
                    filters.push(leaf(Leaf::Ext(ext_set(VIDEO, fold))));
                    consumed_value = false;
                }
                "empty" => {
                    self.q.uses_child_counts = true;
                    filters.push(leaf(Leaf::Empty));
                    consumed_value = false;
                }
                "dupe" => {
                    self.q.dupe = Some(Dupe::Name);
                    consumed_value = false;
                }
                "sizedupe" => {
                    self.q.dupe = Some(Dupe::Size);
                    consumed_value = false;
                }
                "ext" => filters.push(leaf(Leaf::Ext(ext_set(&value, fold)))),
                "size" => {
                    let (lo, hi) =
                        parse_range(&value, true).ok_or_else(|| format!("bad size: {value}"))?;
                    filters.push(leaf(Leaf::Size { lo, hi }));
                }
                "depth" => {
                    let (lo, hi) =
                        parse_range(&value, false).ok_or_else(|| format!("bad depth: {value}"))?;
                    filters.push(leaf(Leaf::Depth { lo, hi }));
                }
                "runcount" => {
                    let (lo, hi) = parse_range(&value, false)
                        .ok_or_else(|| format!("bad run count: {value}"))?;
                    self.q.uses_runs = true;
                    filters.push(leaf(Leaf::RunCount { lo, hi }));
                }
                "attrib" | "attributes" => {
                    let a =
                        parse_attrib(&value).ok_or_else(|| format!("bad attributes: {value}"))?;
                    filters.push(leaf(Leaf::Attrib(a)));
                }
                k @ ("dm" | "datemodified" | "dc" | "datecreated" | "rc" | "recentchange") => {
                    let field = match k {
                        "dm" | "datemodified" => DateField::Modified,
                        "dc" | "datecreated" => DateField::Created,
                        _ => DateField::RecentChange,
                    };
                    if field == DateField::RecentChange {
                        self.q.uses_recent = true;
                    }
                    if field == DateField::Created {
                        self.q.uses_created = true;
                    }
                    let (lo, hi) = if value.is_empty() && field == DateField::RecentChange {
                        (i64::MIN, i64::MAX)
                    } else {
                        parse_date_filter(&value, self.env)
                            .ok_or_else(|| format!("bad date: {value}"))?
                    };
                    filters.push(leaf(Leaf::Date { field, lo, hi }));
                }
                "parent" | "infolder" => {
                    let p = fold_mode(value.trim_end_matches(['\\', '/']), fold).replace('/', "\\");
                    self.q.parents.push(p.into_bytes());
                    filters.push(leaf(Leaf::Parent(self.q.parents.len() - 1)));
                }
                _ => break, // not a modifier (e.g. `c:\windows`)
            }
            s = if consumed_value { "" } else { rest };
            if consumed_value {
                break;
            }
        }

        let mut text = unquote(s);
        if !text.is_empty() {
            if !m.regex {
                text = text.replace('/', "\\");
            }
            let path = m.path || (!m.regex && text.contains('\\'));
            let l = if m.regex {
                let re = regex::bytes::RegexBuilder::new(&text)
                    .case_insensitive(!m.case)
                    .build()
                    .map_err(|e| e.to_string())?;
                Leaf::Regex { re, path }
            } else if text.contains(['*', '?']) || m.exact {
                let mut pat = if m.case {
                    text.into_bytes()
                } else {
                    fold_mode(&text, fold).into_bytes()
                };
                let slot = self.q.path_globs.len();
                if path {
                    pat = path_glob(pat);
                    self.q.path_globs.push((pat.clone(), m.case));
                }
                Leaf::Glob {
                    pat,
                    case: m.case,
                    path,
                    slot,
                }
            } else if path && !m.ww && text.chars().count() <= 60 {
                // A path substring is the wildcard `*text*`, which the
                // per-folder matcher answers without building every path.
                let needle = if m.case { text } else { fold_mode(&text, fold) };
                let pat = format!("*{needle}*").into_bytes();
                let slot = self.q.path_globs.len();
                self.q.path_globs.push((pat.clone(), m.case));
                Leaf::Glob {
                    pat,
                    case: m.case,
                    path,
                    slot,
                }
            } else {
                let needle = if m.case {
                    text.into_bytes()
                } else {
                    fold_mode(&text, fold).into_bytes()
                };
                Leaf::Text {
                    needle,
                    case: m.case,
                    path,
                    whole_word: m.ww,
                }
            };
            filters.push(Node::Leaf(l));
        }

        let node = match filters.len() {
            0 => return Ok(None),
            1 => filters.pop().unwrap(),
            _ => Node::All(filters),
        };
        Ok(Some(if neg { Node::Not(Box::new(node)) } else { node }))
    }
}

impl Query {
    pub fn parse(s: &str) -> Result<Query, String> {
        Self::parse_with(
            s,
            SearchFlags::default(),
            FoldMode::Lower,
            &DateEnv::system(),
        )
    }

    pub fn parse_with(
        s: &str,
        flags: SearchFlags,
        fold: FoldMode,
        env: &DateEnv,
    ) -> Result<Query, String> {
        let mut p = Parser {
            toks: tokenize(s),
            pos: 0,
            defaults: Mods {
                case: flags.case,
                path: flags.path,
                ww: flags.whole_word,
                regex: flags.regex,
                exact: false,
            },
            fold,
            env,
            q: Query {
                root: Node::All(Vec::new()),
                dupe: None,
                parents: Vec::new(),
                fold,
                uses_child_counts: false,
                uses_runs: false,
                uses_recent: false,
                uses_created: false,
                path_globs: Vec::new(),
            },
        };
        let mut items = Vec::new();
        while p.pos < p.toks.len() {
            match p.parse_and()? {
                Node::All(v) => items.extend(v),
                n => items.push(n),
            }
            if p.peek() == Some(&Tok::Close) {
                p.pos += 1; // unbalanced `>`
            }
        }
        p.q.root = Node::All(items);
        Ok(p.q)
    }

    pub fn is_empty(&self) -> bool {
        matches!(&self.root, Node::All(v) if v.is_empty()) && self.dupe.is_none()
    }

    /// True when the whole query is one case-insensitive name substring.
    pub fn is_single_name_substring(&self) -> bool {
        self.dupe.is_none()
            && matches!(&self.root, Node::All(v) if v.len() == 1
            && matches!(&v[0], Node::Leaf(Leaf::Text { case: false, path: false, whole_word: false, .. })))
    }

    /// A folded literal that every match must contain in its *name*, used to
    /// drive the arena scan. Picks the longest available.
    pub fn driver(&self) -> Option<Vec<u8>> {
        let Node::All(items) = &self.root else {
            return None;
        };
        items
            .iter()
            .filter_map(|n| match n {
                Node::Leaf(Leaf::Text {
                    needle,
                    case: false,
                    path: false,
                    ..
                }) => Some(needle.clone()),
                Node::Leaf(Leaf::Glob {
                    pat,
                    case: false,
                    path: false,
                    ..
                }) => pat
                    .split(|&b| is_wild(b))
                    .max_by_key(|p| p.len())
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_vec()),
                Node::Leaf(Leaf::Glob {
                    pat,
                    case: false,
                    path: true,
                    ..
                }) => glob_tail(pat).map(|t| t.to_vec()),
                Node::Leaf(Leaf::Ext(exts)) if exts.len() == 1 => {
                    let mut v = vec![b'.'];
                    v.extend_from_slice(exts.iter().next().unwrap());
                    Some(v)
                }
                _ => None,
            })
            .max_by_key(|v| v.len())
    }

    /// Plain (non-regex) terms, folded, for highlighting matches in a UI.
    pub fn highlight_terms(&self) -> Vec<Vec<u8>> {
        fn walk(n: &Node, neg: bool, out: &mut Vec<Vec<u8>>) {
            match n {
                Node::All(v) | Node::Any(v) => v.iter().for_each(|c| walk(c, neg, out)),
                Node::Not(c) => walk(c, !neg, out),
                Node::Leaf(Leaf::Text {
                    needle,
                    case: false,
                    ..
                }) if !neg => out.push(needle.clone()),
                Node::Leaf(Leaf::Glob {
                    pat, case: false, ..
                }) if !neg => out.extend(
                    pat.split(|&b| is_wild(b))
                        .filter(|p| !p.is_empty())
                        .map(|p| p.to_vec()),
                ),
                _ => {}
            }
        }
        let mut v = Vec::new();
        walk(&self.root, false, &mut v);
        v
    }
}

// ---- evaluation --------------------------------------------------------------

/// Per-index data a query needs beyond the entries themselves; built once per
/// search by [`crate::search`].
#[derive(Default)]
pub struct Prep {
    /// For each `parent:` path, the entries of folders with exactly that path.
    pub parent_sets: Vec<HashSet<u32>>,
    pub child_counts: Option<Arc<Vec<u32>>>,
    /// Entry → run count (only non-zero).
    pub runs: HashMap<u32, u32>,
    /// Entry → time of the last change seen in the journal.
    pub recent: HashMap<u32, i64>,
    /// Matchers for [`Query::path_globs`] (missing: match on built paths).
    pub path_globs: Vec<Option<PathGlob>>,
}

/// Per-entry evaluation context; builds paths lazily and at most once.
pub struct Ctx<'a> {
    pub ix: &'a Index,
    pub i: u32,
    pub prep: &'a Prep,
    fold_path: Option<Vec<u8>>,
    raw_path: Option<Vec<u8>>,
}

impl<'a> Ctx<'a> {
    pub fn new(ix: &'a Index, i: u32, prep: &'a Prep) -> Self {
        Ctx {
            ix,
            i,
            prep,
            fold_path: None,
            raw_path: None,
        }
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

fn char_len(s: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && s[j] & 0xC0 == 0x80 {
        j += 1;
    }
    j - i
}

/// Wildcard match over UTF-8 bytes; `?` consumes one character.
pub fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    if pat.contains(&ANY_DIRS) {
        return glob_match_dirs(pat, s);
    }
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star_p, mut star_i) = (usize::MAX, 0usize);
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

/// [`glob_match`] for patterns with [`ANY_DIRS`]: backtracking with a memo
/// of failed (pattern, text) positions, so it stays O(pattern × text).
fn glob_match_dirs(pat: &[u8], s: &[u8]) -> bool {
    fn go(pat: &[u8], s: &[u8], p: usize, i: usize, failed: &mut [u64]) -> bool {
        let key = p * (s.len() + 1) + i;
        if failed[key / 64] >> (key % 64) & 1 == 1 {
            return false;
        }
        let ok = match pat.get(p) {
            None => i == s.len(),
            Some(b'*') => {
                go(pat, s, p + 1, i, failed)
                    || (i < s.len() && go(pat, s, p, i + char_len(s, i), failed))
            }
            Some(&ANY_DIRS) => {
                go(pat, s, p + 1, i, failed)
                    || (i..s.len()).any(|j| s[j] == b'\\' && go(pat, s, p + 1, j + 1, failed))
            }
            Some(b'?') => i < s.len() && go(pat, s, p + 1, i + char_len(s, i), failed),
            Some(&c) => i < s.len() && s[i] == c && go(pat, s, p + 1, i + 1, failed),
        };
        if !ok {
            failed[key / 64] |= 1 << (key % 64);
        }
        ok
    }
    let mut failed = vec![0u64; ((pat.len() + 1) * (s.len() + 1)).div_ceil(64)];
    go(pat, s, 0, 0, &mut failed)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum GTok {
    Lit(char),
    One,
    Star,
    /// [`ANY_DIRS`] is two states: an entry that can skip ahead (no folders)
    /// and a loop that consumes anything but can only move on after a `\`.
    DirsEntry,
    DirsLoop,
}

/// A path wildcard pattern as an NFA over characters, with the state after
/// each folder's path cached, so an entry only feeds `\` and its own name
/// (and a folder that can't lead to a match prunes everything below it).
pub struct PathGlob {
    toks: Vec<GTok>,
    case: bool,
    start: u64,
    /// Shift-and tables: positions whose token consumes the byte and moves
    /// on (per ASCII byte), or that stay put (`*`, folder loops).
    advance: [u64; 128],
    one: u64,
    stay: u64,
    dirs_loop: u64,
    /// Epsilon closure of each position.
    eps: Vec<u64>,
    /// Per entry: `KNOWN | state` once computed (0 = not yet).
    states: Vec<std::sync::atomic::AtomicU64>,
}

const KNOWN: u64 = 1 << 63;

impl PathGlob {
    /// `None` for patterns too long for a 62-position state set.
    pub fn new(pat: &[u8], case: bool, entries: usize) -> Option<PathGlob> {
        let s = std::str::from_utf8(pat).ok()?;
        let mut toks = Vec::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '*' => toks.push(GTok::Star),
                '?' => toks.push(GTok::One),
                '\u{1}' => toks.extend([GTok::DirsEntry, GTok::DirsLoop]),
                c => toks.push(GTok::Lit(c)),
            }
        }
        if toks.len() > 62 {
            return None;
        }
        let m = toks.len();
        let mut advance = [0u64; 128];
        let (mut one, mut stay, mut dirs_loop) = (0u64, 0u64, 0u64);
        for (p, t) in toks.iter().enumerate() {
            match *t {
                GTok::Lit(c) if (c as u32) < 128 => advance[c as usize] |= 1 << p,
                GTok::One => one |= 1 << p,
                GTok::Star => stay |= 1 << p,
                GTok::DirsLoop => dirs_loop |= 1 << p,
                _ => {}
            }
        }
        // Closures back to front: a position reaches what its epsilon targets reach.
        let mut eps = vec![0u64; m + 1];
        eps[m] = 1 << m;
        for p in (0..m).rev() {
            eps[p] = 1 << p
                | match toks[p] {
                    GTok::Star => eps[p + 1],
                    GTok::DirsEntry => eps[p + 1] | eps.get(p + 2).copied().unwrap_or(0),
                    _ => 0,
                };
        }
        let mut g = PathGlob {
            toks,
            case,
            start: 0,
            advance,
            one,
            stay,
            dirs_loop,
            eps,
            states: Vec::new(),
        };
        g.start = g.closure(1);
        g.states = (0..entries)
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect();
        Some(g)
    }

    fn closure(&self, s: u64) -> u64 {
        let mut out = s;
        let mut bits = s;
        while bits != 0 {
            let p = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            out |= self.eps[p];
        }
        out
    }

    fn step(&self, s: u64, c: char) -> u64 {
        let lit = if (c as u32) < 128 {
            self.advance[c as usize]
        } else {
            self.toks
                .iter()
                .enumerate()
                .filter(|(_, t)| **t == GTok::Lit(c))
                .fold(0, |m, (p, _)| m | 1 << p)
        };
        let mut t = ((s & (lit | self.one)) << 1) | (s & (self.stay | self.dirs_loop));
        if c == '\\' {
            t |= (s & self.dirs_loop) << 1;
        }
        self.closure(t)
    }

    fn feed(&self, mut s: u64, text: &[u8]) -> u64 {
        if text.is_ascii() {
            for &b in text {
                if s == 0 {
                    break;
                }
                s = self.step(s, b as char);
            }
            return s;
        }
        for c in unsafe { std::str::from_utf8_unchecked(text) }.chars() {
            if s == 0 {
                break;
            }
            s = self.step(s, c);
        }
        s
    }

    fn name<'a>(&self, ix: &'a Index, i: u32) -> &'a [u8] {
        if self.case {
            ix.name(i).as_bytes()
        } else {
            ix.fold_name(i)
        }
    }

    /// State after the full path of entry `i`.
    fn state(&self, ix: &Index, i: u32) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let cached = |i: u32| {
            self.states
                .get(i as usize)
                .map(|a| a.load(Relaxed))
                .filter(|v| v & KNOWN != 0)
        };
        if let Some(v) = cached(i) {
            return v & !KNOWN;
        }
        // Climb to the nearest known ancestor, then compute back down.
        let mut chain = Vec::with_capacity(16);
        let mut cur = i;
        let mut s = loop {
            if cur == crate::index::NONE {
                break None;
            }
            if let Some(v) = cached(cur) {
                break Some(v & !KNOWN);
            }
            chain.push(cur);
            if chain.len() > 1024 {
                return 0;
            }
            cur = ix.entries[cur as usize].parent;
        };
        for &c in chain.iter().rev() {
            let next = match s {
                None => self.feed(self.start, self.name(ix, c)),
                Some(st) => self.feed(self.step(st, '\\'), self.name(ix, c)),
            };
            // Only folders' states are reused (by their children).
            if ix.entries[c as usize].is_dir()
                && let Some(a) = self.states.get(c as usize)
            {
                a.store(next | KNOWN, Relaxed);
            }
            s = Some(next);
        }
        s.unwrap_or(0)
    }

    pub fn matches(&self, ix: &Index, i: u32) -> bool {
        self.state(ix, i) >> self.toks.len() & 1 == 1
    }
}

/// Folded extension of a folded name (after the last `.`), if any.
pub fn extension(name: &[u8]) -> Option<&[u8]> {
    let dot = memchr::memrchr(b'.', name)?;
    (dot + 1 < name.len()).then(|| &name[dot + 1..])
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
        let e = &c.ix.entries[c.i as usize];
        match self {
            Leaf::Text {
                needle,
                case,
                path,
                whole_word,
            } => {
                let hay = c.hay(*path, *case);
                if *whole_word {
                    contains_word(hay, needle)
                } else {
                    memchr::memmem::find(hay, needle).is_some()
                }
            }
            Leaf::Glob {
                pat,
                case,
                path: true,
                slot,
            } => match c.prep.path_globs.get(*slot) {
                Some(Some(g)) => g.matches(c.ix, c.i),
                _ => glob_match(pat, c.hay(true, *case)),
            },
            Leaf::Glob {
                pat,
                case,
                path: false,
                ..
            } => glob_match(pat, c.hay(false, *case)),
            Leaf::Regex { re, path } => re.is_match(c.hay(*path, true)),
            Leaf::Ext(exts) => {
                !e.is_dir() && extension(c.ix.fold_name(c.i)).is_some_and(|x| exts.contains(x))
            }
            Leaf::Size { lo, hi } => !e.is_dir() && e.size >= *lo && e.size <= *hi,
            Leaf::IsFile => !e.is_dir(),
            Leaf::IsDir => e.is_dir(),
            Leaf::Date { field, lo, hi } => {
                let t = match field {
                    DateField::Modified => e.modified,
                    DateField::Created => match c.ix.created(c.i) {
                        Some(t) => t,
                        None => return false,
                    },
                    DateField::RecentChange => match c.prep.recent.get(&c.i) {
                        Some(&t) => t,
                        None => return false,
                    },
                };
                t >= *lo && t < *hi
            }
            Leaf::Parent(id) => c
                .prep
                .parent_sets
                .get(*id)
                .is_some_and(|s| s.contains(&e.parent)),
            Leaf::Depth { lo, hi } => {
                let d = c.ix.depth(c.i) as u64;
                d >= *lo && d <= *hi
            }
            Leaf::Empty => {
                e.is_dir()
                    && c.prep
                        .child_counts
                        .as_ref()
                        .is_some_and(|cc| cc[c.i as usize] == 0)
            }
            Leaf::Attrib(m) => e.attrs() & m == *m,
            Leaf::RunCount { lo, hi } => {
                let n = c.prep.runs.get(&c.i).copied().unwrap_or(0) as u64;
                n >= *lo && n <= *hi
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn env() -> DateEnv {
        DateEnv::fixed_utc(2026, 10, 20, 12, 0)
    }

    fn parse(q: &str) -> Query {
        Query::parse_with(q, SearchFlags::default(), FoldMode::Lower, &env()).unwrap()
    }

    fn matches_with(q: &str, prep: &Prep) -> Vec<String> {
        let ix = sample();
        let q = parse(q);
        let mut v: Vec<String> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_visible(i) && q.root.eval(&mut Ctx::new(&ix, i, prep)))
            .map(|i| ix.name(i).to_string())
            .collect();
        v.sort();
        v
    }

    fn matches(q: &str) -> Vec<String> {
        matches_with(q, &Prep::default())
    }

    #[test]
    fn substring_and_or_not() {
        assert_eq!(matches("note"), vec!["notepad.exe", "notes.txt"]);
        assert_eq!(matches("note !exe"), vec!["notes.txt"]);
        assert_eq!(matches("report|notepad"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(
            matches("report | notepad"),
            vec!["Report.PDF", "notepad.exe"]
        );
        assert_eq!(matches("case:report"), Vec::<String>::new());
        assert_eq!(matches("case:Report"), vec!["Report.PDF"]);
    }

    #[test]
    fn grouping_and_precedence() {
        // OR binds tighter: `note pad|txt` = note AND (pad OR txt).
        assert_eq!(matches("note pad|txt"), vec!["notepad.exe", "notes.txt"]);
        assert_eq!(
            matches("<note exe>|report"),
            vec!["Report.PDF", "notepad.exe"]
        );
        assert_eq!(matches("!<note exe> note"), vec!["notes.txt"]);
        assert_eq!(matches("<<note>>"), vec!["notepad.exe", "notes.txt"]);
        // `>` inside a modifier value is not a group close.
        assert_eq!(matches("<size:>100>"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("size:<100"), vec!["notes.txt"]);
    }

    #[test]
    fn wildcards_paths_and_filters() {
        assert_eq!(matches("*.txt"), vec!["notes.txt"]);
        assert_eq!(matches("n?tes.*"), vec!["notes.txt"]);
        assert_eq!(matches(r"users\me\"), vec!["Report.PDF", "notes.txt"]);
        assert_eq!(matches("users/me/"), vec!["Report.PDF", "notes.txt"]);
        assert_eq!(matches("ext:pdf;exe"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("size:>100"), vec!["Report.PDF", "notepad.exe"]);
        assert_eq!(matches("size:1kb..2kb"), Vec::<String>::new());
        assert_eq!(matches("folder: e"), vec!["Me", "Users"]);
        assert_eq!(matches("regex:^no.*t$"), vec!["notes.txt"]);
        assert_eq!(matches("ww:me"), vec!["Me"]);
        assert_eq!(matches(r#""notes.txt""#), vec!["notes.txt"]);
        assert_eq!(matches("exe:"), vec!["notepad.exe"]);
        assert_eq!(matches("doc:"), vec!["Report.PDF", "notes.txt"]);
        assert_eq!(matches("depth:1"), vec!["Users", "Windows"]);
        assert_eq!(matches("depth:>2"), vec!["Report.PDF", "notes.txt"]);
        assert_eq!(matches("attrib:D"), vec!["C:", "Me", "Users", "Windows"]);
    }

    #[test]
    fn path_wildcards_span_folders() {
        assert_eq!(matches(r"users\*\*.pdf"), vec!["Report.PDF"]);
        assert_eq!(matches(r"users\*.pdf"), vec!["Report.PDF"]); // `*` crosses `\me\`
        assert_eq!(matches(r"c:\*\notes.txt"), vec!["notes.txt"]);
        assert_eq!(matches(r"C:\**\*.exe"), vec!["notepad.exe"]);
        assert_eq!(matches(r"users\*"), vec!["Me", "Report.PDF", "notes.txt"]);
        assert_eq!(matches("users/m?/*"), vec!["Report.PDF", "notes.txt"]);
        assert_eq!(matches(r"sers\*\notes.txt"), vec!["notes.txt"]); // starts anywhere
        assert!(matches(r"users\*\me").contains(&"Me".to_string())); // ends at the end
        assert!(matches(r"windows\*\me").is_empty());
        assert_eq!(matches("wfn:me"), vec!["Me"]);
        assert_eq!(matches("wfn:note"), Vec::<String>::new());
        assert_eq!(matches(r"wfn:c:\users\me"), vec!["Me"]);
        assert_eq!(
            parse("wfn:node_modules").driver(),
            Some(b"node_modules".to_vec())
        );
        let g = |p: &str, s: &str| glob_match(&path_glob(p.as_bytes().to_vec()), s.as_bytes());
        assert!(
            g(r"a\*\b", r"x\a\b")
                && g(r"a\*\b", r"a\1\2\b")
                && !g(r"a\*\b", r"a\1b")
                && !g(r"a\*\b", r"a\b\c")
        );
        assert!(g(r"a\*\*\b", r"a\b") && g(r"\*\b?", r"c:\b1") && !g(r"\*\b?", r"c:\b"));
        assert!(g(r"a\*\*b*\*\c", &format!(r"a\{}b\c", r"q\".repeat(200)))); // memoized: no blowup
        // The literal tail drives the name scan.
        assert_eq!(parse(r"users\*\*.pdf").driver(), Some(b".pdf".to_vec()));
        assert_eq!(parse(r"users\*\x\*").driver(), None);
    }

    /// The per-folder NFA must agree with matching the built path.
    #[test]
    fn path_glob_nfa_agrees_with_reference() {
        let ix = sample();
        for q in [
            r"users\*\*.pdf",
            r"users\*",
            r"c:\*\notes.txt",
            r"C:\**\*.exe",
            r"*\m?\*",
            r"sers\*\notes.txt",
            r"users\*\me",
            r"windows\*\me",
            r"c:\*",
            r"*",
            r"c:\users\me\*.txt",
            r"\*\*\*\*\r*",
            r"case:Users\*\R*",
            r"case:users\*",
            r"*e*\*e*",
            r"c:\w*\n?tepad.*",
        ] {
            let parsed = parse(q);
            let nfa = Prep {
                path_globs: parsed
                    .path_globs
                    .iter()
                    .map(|(p, c)| PathGlob::new(p, *c, ix.entries.len()))
                    .collect(),
                ..Default::default()
            };
            assert!(nfa.path_globs.iter().all(Option::is_some), "{q}");
            let a = matches_with(q, &nfa);
            let b = matches_with(q, &Prep::default());
            assert_eq!(a, b, "{q}");
        }
        let long = "a?".repeat(40);
        assert!(PathGlob::new(long.as_bytes(), false, 1).is_none());
    }

    #[test]
    fn path_glob_nfa_fuzz() {
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let alphabet = ['a', 'b', 'å', '\\'];
        let wild = ['a', 'b', 'å', '\\', '*', '?', '\\', '*'];
        for _ in 0..20_000 {
            let path: String = (0..next(12)).map(|_| alphabet[next(4) as usize]).collect();
            let raw: String = (0..1 + next(7)).map(|_| wild[next(8) as usize]).collect();
            let pat = path_glob(raw.clone().into_bytes());
            let g = PathGlob::new(&pat, true, 0).unwrap();
            let nfa = g.feed(g.start, path.as_bytes()) >> g.toks.len() & 1 == 1;
            assert_eq!(
                nfa,
                glob_match(&pat, path.as_bytes()),
                "pattern {raw:?} path {path:?}"
            );
        }
    }

    #[test]
    fn dates() {
        // sample(): modified = record number as FILETIME ticks (i.e. 1601).
        assert_eq!(matches("dm:<1700"), matches("*"));
        assert!(matches("dm:today").is_empty());
        let mut prep = Prep::default();
        let ix = sample();
        let notes = (0..ix.entries.len() as u32)
            .find(|&i| ix.name(i) == "notes.txt")
            .unwrap();
        prep.recent.insert(
            notes,
            crate::date::parse_date_filter("today", &env()).unwrap().0 + 1,
        );
        assert_eq!(matches_with("rc:today", &prep), vec!["notes.txt"]);
        assert_eq!(matches_with("rc:", &prep), vec!["notes.txt"]);
        assert!(
            Query::parse_with("dm:nope", SearchFlags::default(), FoldMode::Lower, &env()).is_err()
        );
    }

    #[test]
    fn parent_empty_runs_via_prep() {
        let ix = sample();
        let q = parse(r"parent:C:\Users\Me\");
        assert_eq!(q.parents, vec![br"c:\users\me".to_vec()]);
        let me = (0..ix.entries.len() as u32)
            .find(|&i| ix.name(i) == "Me")
            .unwrap();
        let mut prep = Prep {
            parent_sets: vec![HashSet::from([me])],
            ..Default::default()
        };
        assert_eq!(
            matches_with(r"parent:C:\Users\Me", &prep),
            vec!["Report.PDF", "notes.txt"]
        );
        let pad = (0..ix.entries.len() as u32)
            .find(|&i| ix.name(i) == "notepad.exe")
            .unwrap();
        prep.runs.insert(pad, 3);
        assert_eq!(matches_with("runcount:>0", &prep), vec!["notepad.exe"]);
        let mut cc = vec![1u32; ix.entries.len()];
        cc[me as usize] = 0;
        prep.child_counts = Some(Arc::new(cc));
        assert_eq!(matches_with("empty:", &prep), vec!["Me"]);
    }

    #[test]
    fn flags_and_dupe_and_tokens() {
        let q = Query::parse_with(
            "Report",
            SearchFlags {
                case: true,
                ..Default::default()
            },
            FoldMode::Lower,
            &env(),
        )
        .unwrap();
        assert!(
            matches!(&q.root, Node::All(v) if matches!(&v[0], Node::Leaf(Leaf::Text { case: true, .. })))
        );
        let q = parse("dupe: foo");
        assert_eq!(q.dupe, Some(Dupe::Name));
        assert!(!q.is_empty());
        assert_eq!(
            SearchFlags::from_bits(
                SearchFlags {
                    path: true,
                    regex: true,
                    ..Default::default()
                }
                .bits()
            ),
            SearchFlags {
                path: true,
                regex: true,
                ..Default::default()
            }
        );
        assert_eq!(
            tokenize("a <b|c> !<d>"),
            vec![
                Tok::Word("a".into()),
                Tok::Open,
                Tok::Word("b".into()),
                Tok::Or,
                Tok::Word("c".into()),
                Tok::Close,
                Tok::Not,
                Tok::Open,
                Tok::Word("d".into()),
                Tok::Close,
            ]
        );
        assert_eq!(parse("ab *.longer").driver(), Some(b".longer".to_vec()));
        assert_eq!(parse("ext:rs").driver(), Some(b".rs".to_vec()));
        assert_eq!(parse("a|b").driver(), None);
        assert_eq!(
            parse("foo !bar *.r?s").highlight_terms(),
            vec![b"foo".to_vec(), b".r".to_vec(), b"s".to_vec()]
        );
    }

    #[test]
    fn diacritics_mode_folds_query_too() {
        let q = Query::parse_with(
            "Café",
            SearchFlags::default(),
            FoldMode::StripDiacritics,
            &env(),
        )
        .unwrap();
        assert_eq!(q.driver(), Some(b"cafe".to_vec()));
    }

    #[test]
    fn glob_handles_unicode_question_mark() {
        assert!(glob_match("h?j".as_bytes(), "håj".as_bytes()));
        assert!(glob_match(b"*", b""));
        assert!(!glob_match(b"a*b", b"ac"));
        assert_eq!(extension(b"a.tar.gz"), Some(&b"gz"[..]));
        assert_eq!(extension(b"noext"), None);
    }
}
