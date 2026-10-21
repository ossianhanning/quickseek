//! The MCP tools: finding files, disk usage, cleanup candidates, deleting.
//!
//! Everything goes through a [`Backend`] (the rapidfind service over its
//! pipe, or an in-process engine in tests), so the index answers instantly
//! and the filesystem is only touched to delete, and to read file contents
//! for `search_files`' `content` (here, with the rights of the user who
//! started the server, capped at [`CONTENT_BUDGET`] bytes per call).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use rapidfind_core::content::{self, ContentOptions, ContentQuery};
use rapidfind_core::fmt;
use rapidfind_core::ipc::{self, Backend, Row, StatusInfo};
use rapidfind_core::query::SearchFlags;
use rapidfind_core::search::SortKey;
use rapidfind_core::tree::{NodeRef, TreeLevel, TreeRow, TreeSort};
use serde_json::{Value, json};

const SYNTAX: &str = "rapidfind query syntax (like Everything): space = AND, `a|b` = OR, `!a` = NOT, `<a b>|c` grouping, \"quoted phrases\". \
Wildcards: `*.mp4` matches whole names; in a path, `*` spans any number of folders and `\\*\\` also matches zero folders \
(`users\\*\\*.pdf`, `C:\\Projects\\*\\node_modules`). A term with `\\` matches the full path. \
Filters: `ext:jpg;png`, `size:>100mb`, `size:1gb..5gb`, `dm:today`, `dm:>=2026-01-01`, `dm:<last90days`, `dc:` (created, if indexed), \
`file:`, `folder:`, `parent:C:\\dir` (direct children), `depth:2`, `empty:` (empty folders), `wfn:name` (exact whole name), \
`dupe:` (same name), `sizedupe:` (same size), `attrib:H`, `regex:^a.*b$`, `case:`, `audio:` `video:` `pic:` `doc:` `exe:` `zip:`.";

/// Most file bytes one `search_files` call reads for content search.
pub const CONTENT_BUDGET: u64 = 1 << 30;
/// Larger files are skipped by content search.
const CONTENT_MAX_FILE: u64 = 32 << 20;

pub fn list() -> Value {
    json!({ "tools": [
        {
            "name": "search_files",
            "title": "Search files",
            "description": format!("Find files and folders on this PC instantly: by name, path wildcards, extension, size, date or contents; the largest, oldest or duplicate files. Searches all indexed drives and network shares (an index of every file name, size and date; no disk scan). \
Combine a free-form `query` with structured filters; they are ANDed. Folder sizes are the total of everything inside. {SYNTAX}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Query in rapidfind syntax, e.g. `*.mkv size:>1gb` or `users\\*\\Downloads\\*.iso`." },
                    "name": { "type": "string", "description": "Name contains this (or matches it, with * and ? wildcards)." },
                    "path": { "type": "string", "description": "Only inside this folder (any depth), e.g. `C:\\Users\\me`." },
                    "extensions": { "type": "array", "items": { "type": "string" }, "description": "File extensions without dots, e.g. [\"mp4\", \"mkv\"]." },
                    "min_size": { "type": ["integer", "string"], "description": "Minimum file size: bytes, or a string like \"500mb\"." },
                    "max_size": { "type": ["integer", "string"], "description": "Maximum file size: bytes, or a string like \"2gb\"." },
                    "modified_after": { "type": "string", "description": "e.g. 2026-01-31, \"last30days\", \"thisyear\"." },
                    "modified_before": { "type": "string", "description": "e.g. 2025-06-01, \"last365days\" (= older than a year)." },
                    "type": { "type": "string", "enum": ["any", "file", "folder"], "default": "any" },
                    "sort": { "type": "string", "enum": ["name", "path", "size", "modified"], "description": "Default: size when a size filter is given, else name." },
                    "descending": { "type": "boolean", "description": "Default: true for size and modified, false otherwise." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "match_case": { "type": "boolean", "default": false },
                    "content": { "type": "string", "description": "Only files whose contents contain this text (case-insensitive unless `match_case`). Slow compared to the rest: \
narrow it down with `path`/`extensions`/`query` first. Reads at most 1 GB or 30 s per call, text-like files first and build/dependency folders skipped (files over 32 MB, binaries and online-only cloud files are skipped); \
`content.complete` in the reply says whether every candidate was read. Word/Excel/PowerPoint documents are read through Windows' document filters. \
`content:text` inside `query` works too." },
                    "content_regex": { "type": "boolean", "default": false, "description": "`content` is a regular expression." },
                    "include_build_folders": { "type": "boolean", "default": false, "description": "Content search skips node_modules, target, .git, .venv, __pycache__, obj, dist, build and similar folders unless this is true." }
                }
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "folder_sizes",
            "title": "Folder sizes",
            "description": format!("What is using disk space: answers \"what has eaten my D: drive\" / \"what is taking space on C:\". Disk usage breakdown (like TreeSize/WinDirStat, but instant from the index): the biggest folders and files under a path, \
with size, share of the parent and file count, expanded `depth` levels. Without `path`, starts at the drives (with their capacity and free space). \
With `query`, only matching files count (e.g. `*.mp4` shows where videos take space; `folder: wfn:node_modules` where node_modules are); \
a matched folder counts with everything inside it. {SYNTAX}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder to break down, e.g. `C:\\Users`. Default: all drives." },
                    "depth": { "type": "integer", "minimum": 1, "maximum": 5, "default": 1 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20, "description": "Entries listed per folder (the rest is summarised)." },
                    "query": { "type": "string", "description": "Only count what matches this query." },
                    "sort": { "type": "string", "enum": ["size", "files", "name", "modified"], "default": "size" },
                    "folders_only": { "type": "boolean", "default": false, "description": "List only folders (files still count toward sizes)." }
                }
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "folder_growth",
            "title": "Folder growth",
            "description": "What changed / grew: answers \"why is my disk filling up\", \"what got bigger this week\", \"where did 20 GB go since yesterday\", \"did the cleanup work\". \
Compares folder sizes now with a daily snapshot (kept for 31 days daily, then weekly and monthly) or a named save point, and lists the folders and files under `path` \
by how much they grew or shrank, with sizes then and now and each drive's free space then and now. `depth` follows the biggest changes further down. \
Snapshots only hold folders of 10 MB or more, so smaller folders show up as new. With `list_snapshots: true` it only lists the snapshots there are.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder to look in, e.g. `D:\\` or `C:\\Users\\me`. Default: all drives." },
                    "days": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 7, "description": "Compare with the snapshot closest to this many days ago." },
                    "since": { "type": "string", "description": "Instead of `days`: a date (`2026-10-01`), or a save point's name or id from `list_snapshots`." },
                    "depth": { "type": "integer", "minimum": 1, "maximum": 4, "default": 2, "description": "Levels listed; below the first, only folders that changed are opened." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10, "description": "Entries listed per folder, biggest change first." },
                    "list_snapshots": { "type": "boolean", "default": false }
                }
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "disk_overview",
            "title": "Disk overview",
            "description": "How full each drive is (start here for disk-space questions): every indexed drive and network share with capacity, free space, space taken by indexed files, file count and index state.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "cleanup_candidates",
            "title": "Cleanup candidates",
            "description": "Clean up / free disk space: finds space that can usually be reclaimed, by category: Recycle Bin, temp folders, build output and dependency folders \
(node_modules, Rust target, __pycache__, ...), package manager caches, crash dumps, old Windows installations, big old downloads, huge logs and \
same-size duplicates. Returns each category's total and its biggest items with paths, so you can review and then delete with delete_paths. \
Nothing is deleted by this tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Only look inside this folder (default: everything indexed)." },
                    "top": { "type": "integer", "minimum": 1, "maximum": 50, "default": 5, "description": "Biggest items listed per category." }
                }
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "delete_paths",
            "title": "Delete files or folders",
            "description": "Deletes files or folders. By default they go to the Recycle Bin (recoverable; the space is freed when the bin is emptied). \
`permanent: true` deletes immediately (required for network shares, which have no Recycle Bin). `dry_run: true` only reports what would happen \
and how much space it is. Refuses volume roots, share roots, Windows system folders, Program Files, user profile roots and system files like pagefile.sys.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 500 },
                    "permanent": { "type": "boolean", "default": false },
                    "dry_run": { "type": "boolean", "default": false }
                },
                "required": ["paths"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
        },
        {
            "name": "empty_recycle_bin",
            "title": "Empty the Recycle Bin",
            "description": "Shows how much the Recycle Bin holds and, unless `dry_run`, empties it (permanently deletes its contents) for one drive or all drives.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "drive": { "type": "string", "description": "e.g. \"C:\" (default: all drives)." },
                    "dry_run": { "type": "boolean", "default": false }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
        }
    ]})
}

pub fn call(be: &mut dyn Backend, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "search_files" => search_files(be, args),
        "folder_sizes" => folder_sizes(be, args),
        "folder_growth" => folder_growth(be, args),
        "disk_overview" => disk_overview(be),
        "cleanup_candidates" => cleanup_candidates(be, args),
        "delete_paths" => delete_paths(be, args),
        "empty_recycle_bin" => empty_recycle_bin(args),
        _ => Err(format!("unknown tool: {name}")),
    }
}

// ---- helpers -------------------------------------------------------------------

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn n(a: &Value, k: &str, default: u64, max: u64) -> u64 {
    a.get(k).and_then(Value::as_u64).unwrap_or(default).min(max)
}

fn b(a: &Value, k: &str) -> Option<bool> {
    a.get(k).and_then(Value::as_bool)
}

/// `"C:\x"` with quotes (paths may have spaces) and a trailing `\` so only
/// what's inside matches.
fn inside(path: &str) -> String {
    let p = path.trim().replace('/', "\\");
    format!("\"{}\\\"", p.trim_end_matches('\\'))
}

fn size_arg(v: &Value) -> Option<String> {
    match v {
        Value::Number(x) => x.as_u64().map(|b| b.to_string()),
        Value::String(t) if !t.trim().is_empty() => Some(t.trim().replace(' ', "")),
        _ => None,
    }
}

fn file_row(r: &Row) -> Value {
    let mut o = json!({
        "path": r.full_path(),
        "type": if r.is_dir { "folder" } else { "file" },
        "size": r.size,
        "size_human": fmt::size(r.size),
        "modified": fmt::time(r.modified),
    });
    if !r.is_dir && r.attrs & 0x1000 != 0 {
        o["online_only"] = json!(true); // a cloud placeholder: takes no space on this disk
    }
    o
}

/// The search query the structured arguments describe.
pub fn build_query(a: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(q) = s(a, "query") {
        parts.push(format!("<{q} >"));
    }
    if let Some(name) = s(a, "name") {
        parts.push(format!("\"{}\"", name.replace('"', "")));
    }
    if let Some(p) = s(a, "path") {
        parts.push(inside(p));
    }
    if let Some(exts) = a.get("extensions").and_then(Value::as_array) {
        let list: Vec<&str> = exts.iter().filter_map(Value::as_str).map(|e| e.trim().trim_start_matches('.')).filter(|e| !e.is_empty()).collect();
        if !list.is_empty() {
            parts.push(format!("ext:{}", list.join(";")));
        }
    }
    if let Some(v) = a.get("min_size").and_then(size_arg) {
        parts.push(format!("size:>={v}"));
    }
    if let Some(v) = a.get("max_size").and_then(size_arg) {
        parts.push(format!("size:<={v}"));
    }
    if let Some(d) = s(a, "modified_after") {
        parts.push(format!("dm:>={d}"));
    }
    if let Some(d) = s(a, "modified_before") {
        parts.push(format!("dm:<{d}"));
    }
    match s(a, "type") {
        Some("file") => parts.push("file:".into()),
        Some("folder") => parts.push("folder:".into()),
        _ => {}
    }
    parts.join(" ")
}

fn sort_arg(a: &Value, sized: bool) -> (SortKey, bool) {
    let key = match s(a, "sort") {
        Some("path") => SortKey::Path,
        Some("size") => SortKey::Size,
        Some("modified") => SortKey::Modified,
        Some(_) => SortKey::Name,
        None if sized => SortKey::Size,
        None => SortKey::Name,
    };
    let desc = b(a, "descending").unwrap_or(matches!(key, SortKey::Size | SortKey::Modified));
    (key, desc)
}

fn search_files(be: &mut dyn Backend, a: &Value) -> Result<Value, String> {
    let q = build_query(a);
    let sized = a.get("min_size").is_some() || a.get("max_size").is_some();
    let (sort, desc) = sort_arg(a, sized);
    let limit = n(a, "limit", 50, 1000).max(1) as u32;
    let offset = n(a, "offset", 0, u64::MAX);
    let flags = SearchFlags { case: b(a, "match_case").unwrap_or(false), ..Default::default() };
    let (q, mut cq) = content::split_query(&q, flags)?;
    if let Some(text) = s(a, "content") {
        let one = ContentQuery::single(text, SearchFlags { regex: b(a, "content_regex").unwrap_or(false), ..flags })?;
        cq.get_or_insert_with(ContentQuery::default).terms.extend(one.terms);
    }
    if let Some(cq) = cq {
        return search_contents(be, &q, flags, sort, desc, offset, limit as usize, cq, b(a, "include_build_folders").unwrap_or(false));
    }
    let mut page = be.search(&q, flags, sort, desc, limit)?;
    if offset > 0 {
        page = be.fetch(offset, limit)?;
    }
    let shown = page.rows.len() as u64;
    Ok(json!({
        "query": q,
        "total": page.total,
        "offset": offset,
        "results": page.rows.iter().map(file_row).collect::<Vec<_>>(),
        "has_more": offset + shown < page.total,
        "elapsed_ms": page.elapsed_us as f64 / 1000.0,
    }))
}

/// Folders content search skips unless asked: dependencies, build output and
/// VCS data, which are huge and almost never what "which files mention X" means.
const SKIP_DIRS: &str = r"!*\node_modules\* !*\target\* !*\.git\* !*\.svn\* !*\.hg\* !*\.venv\* !*\venv\* !*\__pycache__\* !*\obj\* !*\.next\* !*\dist\* !*\build\* !*\.gradle\* !*\.cargo\registry\* !*\site-packages\*";

/// Text-like files, read first so the budget goes to likely matches.
const TEXT_EXT: &str = "ext:txt;md;markdown;rst;log;csv;tsv;json;jsonc;yaml;yml;toml;ini;cfg;conf;env;xml;html;htm;css;scss;less;js;mjs;cjs;ts;tsx;jsx;vue;svelte;py;pyi;rs;go;java;kt;kts;scala;c;h;cc;cpp;cxx;hpp;hh;cs;fs;vb;swift;m;mm;rb;php;pl;lua;r;jl;dart;sh;bash;zsh;ps1;psm1;psd1;bat;cmd;sql;graphql;proto;gradle;cmake;mk;tf;hcl;ipynb;tex;bib;svg;sln;csproj;vcxproj;props;targets;shader;hlsl;glsl;uss;uxml;asmdef;doc;docx;odt;rtf;pdf;xls;xlsx;ods;ppt;pptx;odp;msg;eml";

/// Wall-clock limit of one content search (besides the byte budget).
pub const CONTENT_TIME: std::time::Duration = std::time::Duration::from_secs(30);

#[allow(clippy::too_many_arguments)]
fn search_contents(be: &mut dyn Backend, q: &str, flags: SearchFlags, sort: SortKey, desc: bool, offset: u64, limit: usize, cq: ContentQuery, include_build: bool) -> Result<Value, String> {
    let base = if include_build { format!("<{q} >") } else { format!("<{q} > {SKIP_DIRS}") };
    // Text-like files first, unless the caller chose extensions.
    let chose_ext = q.to_ascii_lowercase().contains("ext:");
    let passes: Vec<String> = if chose_ext { vec![base.clone()] } else { vec![format!("{base} {TEXT_EXT}"), format!("{base} !{TEXT_EXT}")] };
    let cq = Arc::new(cq);
    let what = cq.describe();
    // One more than asked, to know whether there are more.
    let want = offset as usize + limit + 1;
    let started = std::time::Instant::now();
    let (mut rows, mut total, mut st) = (Vec::new(), 0u64, content::Stats::default());
    let mut stopped = None;
    for pq in &passes {
        // Smallest first: within the budget that checks the most files.
        let page = be.search(pq, flags, SortKey::Size, false, 1)?;
        total += page.total;
        let left_bytes = CONTENT_BUDGET.saturating_sub(st.bytes);
        let left_time = CONTENT_TIME.saturating_sub(started.elapsed());
        let opts = ContentOptions { max_size: CONTENT_MAX_FILE, byte_budget: Some(left_bytes.max(1)), time_limit: Some(left_time), ..Default::default() };
        let (r, s) = content::matching_rows(&mut |r| be.call(r), cq.clone(), opts, Some(want - rows.len().min(want)))?;
        rows.extend(r);
        for (dst, src) in [(&mut st.checked, s.checked), (&mut st.matched, s.matched), (&mut st.bytes, s.bytes), (&mut st.too_large, s.too_large), (&mut st.binary, s.binary), (&mut st.offline, s.offline), (&mut st.unreadable, s.unreadable)] {
            *dst += src;
        }
        if s.budget_exhausted || s.timed_out || !s.done {
            stopped = Some(if s.budget_exhausted { "budget" } else if s.timed_out { "time" } else { "enough" });
            break;
        }
    }
    let complete = stopped.is_none();
    let key = |r: &Row| r.full_path().to_lowercase();
    match sort {
        SortKey::Size => rows.sort_by_key(|r| r.size),
        SortKey::Modified => rows.sort_by_key(|r| r.modified),
        SortKey::Path => rows.sort_by_key(key),
        _ => rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| key(a).cmp(&key(b)))),
    }
    if desc {
        rows.reverse();
    }
    let shown: Vec<Value> = rows.iter().skip(offset as usize).take(limit).map(file_row).collect();
    let has_more = rows.len() > offset as usize + limit || !complete;
    let mut info = json!({
        "contains": what,
        "candidates": total,
        "checked": st.checked,
        "matched": st.matched,
        "bytes_read": st.bytes,
        "complete": complete,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "order": if chose_ext { "smallest files first" } else { "text-like files (source, documents, config) first, then the rest" },
    });
    if !include_build {
        info["skipped_folders"] = json!("node_modules, target, .git, .venv, __pycache__, obj, dist, build and similar (include_build_folders: true searches them too)");
    }
    if !st.skipped().is_empty() {
        info["skipped"] = json!(st.skipped());
    }
    let incomplete = |why: String| format!("NOT every file was searched: {why}, with {} of {} candidates checked. Results are incomplete; narrow it with `extensions` and a deeper `path` and search again.", st.checked, total);
    match stopped {
        Some("budget") => info["note"] = json!(incomplete(format!("stopped after reading {} (the per-call limit)", fmt::size(CONTENT_BUDGET)))),
        Some("time") => info["note"] = json!(incomplete(format!("stopped after {} s (the per-call limit)", CONTENT_TIME.as_secs()))),
        Some(_) => info["note"] = json!("Stopped once enough matches were found; use `offset` for more."),
        None => {}
    }
    Ok(json!({
        "query": q,
        // Exact once every candidate was read.
        "total": if complete { st.matched } else { rows.len() as u64 },
        "total_is_exact": complete,
        "offset": offset,
        "results": shown,
        "has_more": has_more,
        "content": info,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
    }))
}

fn tree_sort(a: &Value) -> (TreeSort, bool) {
    match s(a, "sort") {
        Some("name") => (TreeSort::Name, false),
        Some("files") => (TreeSort::Files, true),
        Some("modified") => (TreeSort::Modified, true),
        _ => (TreeSort::Size, true),
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { (part as f64 * 1000.0 / whole as f64).round() / 10.0 }
}

fn node_ref(r: &TreeRow) -> NodeRef {
    NodeRef { vol: r.vol.clone(), idx: r.idx, epoch: r.epoch, path: r.full_path() }
}

struct Expand {
    depth: u64,
    limit: u64,
    folders_only: bool,
    sort: TreeSort,
    desc: bool,
    filtered: bool,
}

impl Expand {
    fn rows(&self, be: &mut dyn Backend, level: &TreeLevel, parent_size: u64, depth: u64) -> Result<Value, String> {
        let mut out = Vec::new();
        let mut listed = (0u64, 0u64);
        for r in level.rows.iter().filter(|r| r.is_dir || !self.folders_only).take(self.limit as usize) {
            listed.0 += r.size;
            listed.1 += 1;
            let mut o = json!({
                "name": r.name,
                "path": r.full_path(),
                "type": if r.is_dir { "folder" } else { "file" },
                "size": r.size,
                "size_human": fmt::size(r.size),
                "percent_of_parent": pct(r.size, if r.capacity > 0 { r.capacity } else { parent_size }),
            });
            if r.is_dir {
                o["files"] = json!(r.files);
            }
            if r.cloud > 0 {
                o["online_only"] = json!(r.cloud);
                o["online_only_human"] = json!(fmt::size(r.cloud));
            }
            if r.hardlink {
                o["hard_link"] = json!("another name of a file counted elsewhere; adds no space");
            }
            if r.modified != 0 {
                o["modified"] = json!(fmt::time(r.modified));
            }
            if self.filtered && r.matched {
                o["matched"] = json!(true);
            }
            if r.capacity > 0 {
                o["capacity"] = json!(r.capacity);
                o["free"] = json!(r.free);
                o["free_human"] = json!(fmt::size(r.free));
                o["capacity_human"] = json!(fmt::size(r.capacity));
            }
            if r.is_dir && r.has_children && depth + 1 < self.depth {
                let count = if self.folders_only { 100_000 } else { self.limit as u32 };
                let l = be.children(&node_ref(r), self.sort, self.desc, 0, count)?;
                o["children"] = self.rows(be, &l, r.size, depth + 1)?;
            }
            out.push(o);
        }
        let shown = level.rows.iter().filter(|r| r.is_dir || !self.folders_only).count() as u64;
        let total = if self.folders_only { shown } else { level.total };
        if total > listed.1 {
            out.push(json!({
                "name": format!("({} more)", total - listed.1),
                "size": level.size.saturating_sub(listed.0),
                "size_human": fmt::size(level.size.saturating_sub(listed.0)),
                "note": "the rest of this folder, not listed",
            }));
        }
        Ok(Value::Array(out))
    }
}

fn folder_sizes(be: &mut dyn Backend, a: &Value) -> Result<Value, String> {
    let (sort, desc) = tree_sort(a);
    let query = s(a, "query").unwrap_or("");
    let roots = be.tree(query, SearchFlags::default(), sort, desc)?;
    let ex = Expand { depth: n(a, "depth", 1, 5).max(1), limit: n(a, "limit", 20, 200).max(1), folders_only: b(a, "folders_only").unwrap_or(false), sort, desc, filtered: !query.is_empty() };
    let res = (|| {
        Ok(match s(a, "path") {
            Some(p) => {
                let count = if ex.folders_only { 100_000 } else { ex.limit as u32 };
                let l = be.children(&NodeRef { path: p.to_string(), ..Default::default() }, sort, desc, 0, count)?;
                json!({ "path": p, "size": l.size, "size_human": fmt::size(l.size), "files": l.files, "query": query, "children": ex.rows(be, &l, l.size, 0)? })
            }
            None => json!({ "path": "(all drives)", "size": roots.size, "size_human": fmt::size(roots.size), "online_only": roots.cloud, "files": roots.files, "query": query, "children": ex.rows(be, &roots, roots.size, 0)? }),
        })
    })();
    be.tree_close();
    res.map(|mut v: Value| {
        v["note"] = json!("`size` is what files take on this disk by their length: online-only cloud placeholders (OneDrive etc.) are reported separately as `online_only`, and a file with several hard links counts once. Compressed/sparse files count at their full length.");
        v
    })
}

fn unix_time(t: i64) -> String {
    fmt::time((t + 11_644_473_600) * 10_000_000)
}

fn signed(d: i64) -> String {
    if d < 0 { format!("-{}", fmt::size(d.unsigned_abs())) } else { format!("+{}", fmt::size(d as u64)) }
}

/// `YYYY-MM-DD` as unix seconds at local-ish midnight (UTC; snapshots are a day apart, so close enough).
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || it.next().is_some() {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400)
}

struct Growth {
    depth: u64,
    limit: u64,
}

impl Growth {
    fn rows(&self, be: &mut dyn Backend, level: &TreeLevel, depth: u64) -> Result<Value, String> {
        let mut out = Vec::new();
        let mut listed = 0i64;
        let changed: Vec<&TreeRow> = level.rows.iter().filter(|r| r.change() != 0).collect();
        for r in changed.iter().take(self.limit as usize) {
            listed += r.change();
            let mut o = json!({
                "name": r.name,
                "path": r.full_path(),
                "type": if r.is_dir { "folder" } else { "file" },
                "change": r.change(),
                "change_human": signed(r.change()),
                "size_then": r.before,
                "size_now": r.size,
                "then_human": fmt::size(r.before),
                "now_human": fmt::size(r.size),
            });
            if r.changed {
                o["note"] = json!("new or modified since then");
            } else if r.is_dir && r.before == 0 {
                o["note"] = json!("new, or under 10 MB then");
            }
            if r.capacity > 0 {
                o["free_then"] = json!(fmt::size(r.free_before));
                o["free_now"] = json!(fmt::size(r.free));
                o["capacity"] = json!(fmt::size(r.capacity));
            }
            if r.is_dir && r.has_children && depth + 1 < self.depth {
                let l = be.children(&node_ref(r), TreeSort::Change, true, 0, 100_000)?;
                o["children"] = self.rows(be, &l, depth + 1)?;
            }
            out.push(o);
        }
        let rest = (level.size as i64 - level.before as i64) - listed;
        if changed.len() as u64 > self.limit && rest != 0 {
            out.push(json!({
                "name": format!("({} more changed)", changed.len() as u64 - self.limit),
                "change": rest,
                "change_human": signed(rest),
            }));
        }
        Ok(Value::Array(out))
    }
}

fn folder_growth(be: &mut dyn Backend, a: &Value) -> Result<Value, String> {
    let list = be.history()?;
    let snap = |s: &rapidfind_core::history::SnapInfo| {
        let mut o = json!({ "id": s.id, "time": unix_time(s.time), "folders": s.folders });
        if s.is_save_point() {
            o["save_point"] = json!(if s.name.is_empty() { "(unnamed)" } else { &s.name });
        }
        o
    };
    if b(a, "list_snapshots").unwrap_or(false) {
        return Ok(json!({ "snapshots": list.iter().rev().map(snap).collect::<Vec<_>>() }));
    }
    if list.is_empty() {
        return Err("no snapshots yet: the service takes one a day, the first soon after it starts".into());
    }
    let now = rapidfind_core::history::now_unix();
    let base = match s(a, "since") {
        Some(w) => match list.iter().find(|s| s.id == w || (!s.name.is_empty() && s.name.eq_ignore_ascii_case(w))) {
            Some(s) => s,
            None => {
                let t = parse_date(w).ok_or_else(|| format!("`since`: no save point called \"{w}\" and not a date like 2026-10-01"))?;
                // The first snapshot of that day, else the closest.
                list.iter().filter(|s| s.time >= t && s.time < t + 86_400).min_by_key(|s| s.time).or_else(|| list.iter().min_by_key(|s| (s.time - t).abs())).unwrap()
            }
        },
        None => {
            let target = now - n(a, "days", 7, 1000).max(1) as i64 * 86_400;
            list.iter().min_by_key(|s| (s.time - target).abs()).unwrap()
        }
    };
    let g = Growth { depth: n(a, "depth", 2, 4).max(1), limit: n(a, "limit", 10, 100).max(1) };
    be.compare(&base.id)?;
    let res = (|| {
        let roots = be.tree("", SearchFlags::default(), TreeSort::Change, true)?;
        let (path, level) = match s(a, "path") {
            Some(p) => (p.to_string(), be.children(&NodeRef { path: p.to_string(), ..Default::default() }, TreeSort::Change, true, 0, 100_000)?),
            None => ("(all drives)".to_string(), roots),
        };
        let change = level.size as i64 - level.before as i64;
        Ok::<_, String>(json!({
            "path": path,
            "compared_with": snap(base),
            "days_ago": ((now - base.time) as f64 / 86_400.0 * 10.0).round() / 10.0,
            "size_then": fmt::size(level.before),
            "size_now": fmt::size(level.size),
            "change": change,
            "change_human": signed(change),
            "children": g.rows(be, &level, 0)?,
            "note": "Sizes come from the index now and from the snapshot then; folders under 10 MB aren't in snapshots. Free space can also change from things outside the index (shadow copies, page file, other drives' data).",
        }))
    })();
    let _ = be.compare("");
    be.tree_close();
    res
}

/// Problems the service reports for its volumes, e.g. an index it can't
/// save because the disk is full (searches still work; changes since the
/// last save are lost if the service restarts before it can save).
pub fn index_warnings(st: &StatusInfo) -> Vec<String> {
    st.volumes.iter().filter(|v| v.state.contains(ipc::SAVE_FAILED)).map(|v| format!("{}: {}", v.key, v.state)).collect()
}

fn disk_overview(be: &mut dyn Backend) -> Result<Value, String> {
    let status = be.status().ok_or("the rapidfind service isn't answering")?;
    let roots = be.tree("", SearchFlags::default(), TreeSort::Name, false);
    be.tree_close();
    let roots = roots.unwrap_or_default();
    let vols: Vec<Value> = status
        .volumes
        .iter()
        .map(|v| {
            let root = roots.rows.iter().find(|r| r.vol == v.key);
            let mut o = json!({ "volume": v.key, "kind": v.kind, "state": v.state, "entries": v.entries });
            if let Some(r) = root {
                o["indexed_size"] = json!(r.size);
                o["indexed_size_human"] = json!(fmt::size(r.size));
                o["files"] = json!(r.files);
                if r.capacity > 0 {
                    o["capacity"] = json!(r.capacity);
                    o["capacity_human"] = json!(fmt::size(r.capacity));
                    o["free"] = json!(r.free);
                    o["free_human"] = json!(fmt::size(r.free));
                    o["used_percent"] = json!(pct(r.capacity - r.free.min(r.capacity), r.capacity));
                }
                if r.cloud > 0 {
                    o["online_only"] = json!(r.cloud);
                    o["online_only_human"] = json!(fmt::size(r.cloud));
                }
            }
            o
        })
        .collect();
    let mut out = json!({ "ready": status.ready, "volumes": vols });
    let warnings = index_warnings(&status);
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

// ---- cleanup -------------------------------------------------------------------

struct Category {
    id: &'static str,
    title: &'static str,
    query: &'static str,
    hint: &'static str,
}

const CATEGORIES: &[Category] = &[
    Category { id: "recycle_bin", title: "Recycle Bin", query: r"folder: wfn:$Recycle.Bin", hint: "Use empty_recycle_bin." },
    Category {
        id: "temp",
        title: "Temporary files",
        query: r"folder: <*\appdata\local\temp|*:\windows\temp>",
        hint: "Safe to delete contents not in use; programs that are running may hold some files.",
    },
    Category {
        id: "build_output",
        title: "Build output and dependency folders",
        query: r"folder: <wfn:node_modules|wfn:__pycache__|wfn:.pytest_cache|wfn:.mypy_cache|wfn:.tox|wfn:.next|wfn:.nuxt|wfn:.parcel-cache|wfn:.angular|wfn:.turbo>",
        hint: "Regenerated by the project's build/install command (npm install, pip, ...). Delete for projects you're not working on.",
    },
    Category {
        id: "package_caches",
        title: "Package manager caches",
        query: r"folder: <*\appdata\local\npm-cache|*\appdata\roaming\npm-cache|*\appdata\local\pip\cache|*\appdata\local\yarn\cache|*\appdata\local\pnpm\store|*\.cargo\registry|*\.nuget\packages|*\.gradle\caches|*\.m2\repository|*\go\pkg\mod|*\appdata\local\ms-playwright|*\appdata\local\electron\cache|*\appdata\local\nvidia\dxcache>",
        hint: "Downloaded again when needed (slower next install/build).",
    },
    Category {
        id: "crash_dumps",
        title: "Crash dumps and error reports",
        query: r"<file: ext:dmp;mdmp;hdmp !attrib:O>|<folder: *\microsoft\windows\wer>|<folder: *\appdata\local\crashdumps>",
        hint: "Only needed to debug past crashes.",
    },
    Category {
        id: "old_windows",
        title: "Previous Windows installations",
        query: r"folder: <*:\windows.old|*:\$windows.~bt|*:\$windows.~ws>",
        hint: "Remove with Settings → System → Storage → Temporary files (\"Previous Windows installation\"), which needs administrator rights.",
    },
    Category {
        id: "old_downloads",
        title: "Big downloads older than 90 days",
        query: r"file: *\downloads\* size:>100mb dm:<last90days !attrib:O",
        hint: "Installers and archives that are usually not needed after use.",
    },
    Category { id: "big_logs", title: "Large log files", query: r"file: ext:log;etl size:>50mb !attrib:O", hint: "Logs can usually be deleted; the app starts a new one." },
    Category {
        id: "system_files",
        title: "System files (informational)",
        query: r"file: <*:\hiberfil.sys|*:\pagefile.sys|*:\swapfile.sys>",
        hint: "Not deleted directly: hibernation (powercfg /h off) and page file size are system settings.",
    },
];

/// Every row a query matches (up to 200,000), biggest first.
fn all_rows(be: &mut dyn Backend, q: &str) -> Result<Vec<Row>, String> {
    let mut page = be.search(q, SearchFlags::default(), SortKey::Size, true, 10_000)?;
    let want = page.total.min(200_000);
    while (page.rows.len() as u64) < want {
        let more = be.fetch(page.rows.len() as u64, 10_000)?;
        if more.rows.is_empty() {
            break;
        }
        page.rows.extend(more.rows);
    }
    Ok(page.rows)
}

/// Drops rows inside other rows (a node_modules inside a node_modules),
/// keeping size order; returns them with their total.
fn top_level(rows: Vec<Row>) -> (Vec<Row>, u64) {
    let mut by_path: Vec<(String, usize)> = rows.iter().enumerate().map(|(i, r)| (r.full_path().to_lowercase(), i)).collect();
    by_path.sort();
    let mut keep = vec![false; rows.len()];
    let mut last: Option<String> = None;
    for (p, i) in by_path {
        if let Some(l) = &last
            && p.starts_with(l.as_str())
        {
            continue;
        }
        keep[i] = true;
        last = Some(format!("{}\\", p.trim_end_matches('\\')));
    }
    let kept: Vec<Row> = rows.into_iter().zip(keep).filter_map(|(r, k)| k.then_some(r)).collect();
    let total = kept.iter().map(|r| r.size).sum();
    (kept, total)
}

fn cleanup_candidates(be: &mut dyn Backend, a: &Value) -> Result<Value, String> {
    let top = n(a, "top", 5, 50).max(1) as usize;
    let scope = s(a, "path").map(inside);
    let scoped = |q: &str| match &scope {
        Some(p) => format!("<{q} > {p}"),
        None => q.to_string(),
    };
    let mut cats: Vec<Value> = Vec::new();
    let mut grand = 0u64;
    for c in CATEGORIES {
        let q = scoped(c.query);
        let (items, total) = top_level(all_rows(be, &q)?);
        if total == 0 {
            continue;
        }
        if c.id != "system_files" {
            grand += total;
        }
        cats.push(json!({
            "id": c.id,
            "title": c.title,
            "total_size": total,
            "total_human": fmt::size(total),
            "items": items.len(),
            "query": q,
            "hint": c.hint,
            "biggest": items.iter().take(top).map(file_row).collect::<Vec<_>>(),
        }));
    }
    // Rust build output: `target` folders that contain a CACHEDIR.TAG.
    let tags = be.search(&scoped("file: wfn:CACHEDIR.TAG"), SearchFlags::default(), SortKey::Name, false, 10_000)?;
    let dirs: HashSet<String> = tags.rows.iter().map(|r| r.dir.to_lowercase()).collect();
    if !dirs.is_empty() {
        let targets = be.search(&scoped("folder: wfn:target"), SearchFlags::default(), SortKey::Size, true, 10_000)?;
        let rows: Vec<&Row> = targets.rows.iter().filter(|r| dirs.contains(&r.full_path().to_lowercase())).collect();
        let total: u64 = rows.iter().map(|r| r.size).sum();
        if total > 0 {
            grand += total;
            cats.push(json!({
                "id": "rust_target",
                "title": "Rust build output (target folders)",
                "total_size": total,
                "total_human": fmt::size(total),
                "folders": rows.len(),
                "hint": "Rebuilt by cargo build (or remove with cargo clean in the project).",
                "biggest": rows.iter().take(top).map(|r| file_row(r)).collect::<Vec<_>>(),
            }));
        }
    }
    // Same-size big files: likely duplicates (verify before deleting).
    let dupes = be.search(&scoped("file: sizedupe: size:>=100mb !attrib:O"), SearchFlags::default(), SortKey::Size, true, 5_000)?;
    let mut groups: BTreeMap<std::cmp::Reverse<u64>, Vec<&Row>> = BTreeMap::new();
    for r in &dupes.rows {
        groups.entry(std::cmp::Reverse(r.size)).or_default().push(r);
    }
    let reclaim: u64 = groups.iter().map(|(sz, g)| sz.0 * (g.len() as u64 - 1)).sum();
    if reclaim > 0 {
        cats.push(json!({
            "id": "same_size_files",
            "title": "Big files with identical sizes (possible duplicates)",
            "total_size": reclaim,
            "total_human": fmt::size(reclaim),
            "note": "Space freed by keeping one file per size; same size doesn't prove same content, so compare before deleting.",
            "groups": groups.iter().take(top).map(|(sz, g)| json!({ "size": sz.0, "size_human": fmt::size(sz.0), "paths": g.iter().map(|r| r.full_path()).collect::<Vec<_>>() })).collect::<Vec<_>>(),
        }));
    }
    cats.sort_by_key(|c| std::cmp::Reverse(c["total_size"].as_u64().unwrap_or(0)));
    Ok(json!({
        "total_reclaimable": grand,
        "total_reclaimable_human": fmt::size(grand),
        "categories": cats,
        "note": "Sizes are file sizes from the index (not size on disk); online-only cloud files are left out. Review before deleting; delete_paths moves to the Recycle Bin by default.",
    }))
}

// ---- deleting ------------------------------------------------------------------

fn env_lower(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim_end_matches('\\').to_lowercase())
}

/// Why `path` must not be deleted, if it mustn't.
pub fn protected(path: &str) -> Option<String> {
    let l = path.trim_end_matches('\\').replace('/', "\\").to_lowercase();
    let segs: Vec<&str> = l.split('\\').filter(|s| !s.is_empty()).collect();
    if l.starts_with(r"\\") {
        if segs.len() <= 2 {
            return Some("a network share root".into());
        }
    } else if segs.len() <= 1 {
        return Some("a whole volume".into());
    }
    let under = |base: &str| l == base || l.starts_with(&format!("{base}\\"));
    let sysroot = env_lower("SystemRoot").unwrap_or_else(|| r"c:\windows".into());
    if under(&sysroot) && !l.starts_with(&format!("{sysroot}\\temp\\")) {
        return Some("inside the Windows folder".into());
    }
    for k in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(p) = env_lower(k)
            && l == p
        {
            return Some("the Program Files folder".into());
        }
    }
    if env_lower("ProgramData").is_some_and(|p| l == p) {
        return Some("the ProgramData folder".into());
    }
    // C:\Users and every profile root.
    if segs.len() <= 3 && segs.get(1) == Some(&"users") && !l.starts_with(r"\\") {
        return Some("a user profile folder".into());
    }
    if l.contains(r"\$recycle.bin") {
        return Some("inside the Recycle Bin (use empty_recycle_bin)".into());
    }
    if segs.len() == 2 && ["pagefile.sys", "hiberfil.sys", "swapfile.sys"].contains(&segs[1]) {
        return Some("a system file (hibernation/page file settings control it)".into());
    }
    if segs.get(1).is_some_and(|s| *s == "system volume information") {
        return Some("System Volume Information".into());
    }
    None
}

fn wide_multi(paths: &[&str]) -> Vec<u16> {
    let mut v = Vec::new();
    for p in paths {
        v.extend(p.encode_utf16());
        v.push(0);
    }
    v.push(0);
    v
}

/// Moves to the Recycle Bin (shell, no UI).
fn recycle(path: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW};
    let from = wide_multi(&[path]);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(format!("the shell couldn't recycle it (error {rc:#x}; in use or access denied?)"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("aborted".into());
    }
    Ok(())
}

/// Index size of each path (folders: everything inside), looked up in one tree.
fn sizes_of(be: &mut dyn Backend, paths: &[String]) -> Vec<Option<u64>> {
    if be.tree("", SearchFlags::default(), TreeSort::Name, false).is_err() {
        return vec![None; paths.len()];
    }
    let out = paths
        .iter()
        .map(|p| {
            let p = p.trim_end_matches('\\');
            let (parent, name) = p.rsplit_once('\\')?;
            let l = be.children(&NodeRef { path: parent.to_string(), ..Default::default() }, TreeSort::Name, false, 0, u32::MAX).ok()?;
            l.rows.iter().find(|r| r.name.eq_ignore_ascii_case(name)).map(|r| r.size)
        })
        .collect();
    be.tree_close();
    out
}

fn delete_paths(be: &mut dyn Backend, a: &Value) -> Result<Value, String> {
    let paths: Vec<String> = a
        .get("paths")
        .and_then(Value::as_array)
        .ok_or("`paths` must be a list of paths")?
        .iter()
        .filter_map(Value::as_str)
        .map(|p| p.trim().replace('/', "\\"))
        .filter(|p| !p.is_empty())
        .collect();
    if paths.is_empty() || paths.len() > 500 {
        return Err("give 1 to 500 paths".into());
    }
    let permanent = b(a, "permanent").unwrap_or(false);
    let dry = b(a, "dry_run").unwrap_or(false);
    let sizes = sizes_of(be, &paths);
    let (mut recycled, mut freed, mut failed) = (0u64, 0u64, 0usize);
    let mut results = Vec::new();
    for (p, size) in paths.iter().zip(sizes) {
        let meta = std::fs::symlink_metadata(p);
        let size = size.or_else(|| meta.as_ref().ok().filter(|m| m.is_file()).map(|m| m.len())).unwrap_or(0);
        let mut r = json!({ "path": p, "size": size, "size_human": fmt::size(size) });
        let refuse = if meta.is_err() {
            Some("doesn't exist (already deleted?)".to_string())
        } else if let Some(why) = protected(p) {
            Some(format!("refused: {why}"))
        } else if p.starts_with(r"\\") && !permanent {
            Some("refused: network shares have no Recycle Bin; pass permanent: true to delete it for good".into())
        } else {
            None
        };
        if let Some(why) = refuse {
            r["ok"] = json!(false);
            r["error"] = json!(why);
            failed += 1;
            results.push(r);
            continue;
        }
        let action = if permanent { "deleted permanently" } else { "moved to the Recycle Bin" };
        if dry {
            r["ok"] = json!(true);
            r["action"] = json!(format!("would be {action}"));
        } else {
            let res = if permanent {
                let m = meta.as_ref().unwrap();
                if m.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }.map_err(|e| e.to_string())
            } else {
                recycle(p)
            };
            match res {
                Ok(()) => {
                    r["ok"] = json!(true);
                    r["action"] = json!(action);
                    if permanent {
                        freed += size;
                    } else {
                        recycled += size;
                    }
                }
                Err(e) => {
                    r["ok"] = json!(false);
                    r["error"] = json!(e);
                    failed += 1;
                }
            }
        }
        results.push(r);
    }
    let would: u64 = results.iter().filter(|r| r["ok"] == json!(true)).map(|r| r["size"].as_u64().unwrap_or(0)).sum();
    Ok(json!({
        "dry_run": dry,
        "results": results,
        "failed": failed,
        "freed_now": freed,
        "freed_now_human": fmt::size(freed),
        "moved_to_recycle_bin": recycled,
        "moved_to_recycle_bin_human": fmt::size(recycled),
        "total_human": fmt::size(if dry { would } else { freed + recycled }),
        "note": if !dry && recycled > 0 { "Space in the Recycle Bin is freed when it's emptied (empty_recycle_bin)." } else { "" },
    }))
}

fn empty_recycle_bin(a: &Value) -> Result<Value, String> {
    use windows_sys::Win32::UI::Shell::{SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW, SHQUERYRBINFO, SHQueryRecycleBinW};
    let drive = s(a, "drive").map(|d| format!("{}\\", d.trim_end_matches('\\')));
    let root: Vec<u16> = drive.as_deref().map(|d| d.encode_utf16().chain(Some(0)).collect()).unwrap_or_else(|| vec![0]);
    let root_ptr = if drive.is_some() { root.as_ptr() } else { std::ptr::null() };
    let mut info: SHQUERYRBINFO = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHQUERYRBINFO>() as u32;
    let hr = unsafe { SHQueryRecycleBinW(root_ptr, &mut info) };
    if hr < 0 {
        return Err(format!("couldn't query the Recycle Bin (HRESULT {hr:#x})"));
    }
    let (size, items) = (info.i64Size as u64, info.i64NumItems as u64);
    let scope = drive.clone().unwrap_or_else(|| "all drives".into());
    if b(a, "dry_run").unwrap_or(false) || items == 0 {
        return Ok(json!({ "scope": scope, "size": size, "size_human": fmt::size(size), "items": items, "emptied": false }));
    }
    let hr = unsafe { SHEmptyRecycleBinW(std::ptr::null_mut(), root_ptr, SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND) };
    if hr < 0 {
        return Err(format!("emptying the Recycle Bin failed (HRESULT {hr:#x})"));
    }
    Ok(json!({ "scope": scope, "freed": size, "freed_human": fmt::size(size), "items": items, "emptied": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_from_arguments() {
        let a = json!({ "query": "a|b", "name": "re*port", "path": "C:/Users/me/", "extensions": [".PDF", "docx"], "min_size": "10 mb", "max_size": 5000, "modified_after": "2026-01-01", "type": "file" });
        assert_eq!(build_query(&a), r#"<a|b > "re*port" "C:\Users\me\" ext:PDF;docx size:>=10mb size:<=5000 dm:>=2026-01-01 file:"#);
        assert_eq!(build_query(&json!({})), "");
        assert_eq!(sort_arg(&json!({}), true), (SortKey::Size, true));
        assert_eq!(sort_arg(&json!({ "sort": "name" }), true), (SortKey::Name, false));
    }

    #[test]
    fn content_search() {
        let dir = std::env::temp_dir().join(format!("rapidfind-mcp-content-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello needle world").unwrap();
        std::fs::write(dir.join("b.txt"), "nothing here").unwrap();
        let mut w16 = vec![0xFF, 0xFE];
        w16.extend("utf-16 Needle".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(dir.join("sub").join("c.log"), w16).unwrap();
        let ix = rapidfind_core::folder::build(&dir.to_string_lossy(), rapidfind_core::fold::FoldMode::Lower);
        let mut be = rapidfind_core::ipc::Local::new(Arc::new(rapidfind_core::engine::Engine::from_indexes(vec![ix])));
        let paths = |v: &Value| v["results"].as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap().rsplit('\\').next().unwrap().to_string()).collect::<Vec<_>>();
        let v = call(&mut be, "search_files", &json!({ "content": "needle" })).unwrap();
        assert_eq!(paths(&v), ["a.txt", "c.log"], "{v}");
        assert_eq!(v["content"]["complete"], json!(true));
        assert_eq!(v["total"], json!(2));
        let v = call(&mut be, "search_files", &json!({ "content": "needle", "match_case": true })).unwrap();
        assert_eq!(paths(&v), ["a.txt"]);
        let v = call(&mut be, "search_files", &json!({ "query": "*.txt !content:needle" })).unwrap();
        assert_eq!(paths(&v), ["b.txt"]);
        let v = call(&mut be, "search_files", &json!({ "content": "ne+dle", "content_regex": true, "limit": 1 })).unwrap();
        assert_eq!((paths(&v), v["has_more"].clone()), (vec!["a.txt".to_string()], json!(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn growth_since_a_snapshot() {
        use rapidfind_core::engine::Engine;
        use rapidfind_core::tree::Tree;
        let root = std::env::temp_dir().join(format!("rapidfind-mcp-growth-{}", std::process::id()));
        let (dir, hist) = (root.join("data"), root.join("history"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(dir.join("grows")).unwrap();
        std::fs::create_dir_all(dir.join("same")).unwrap();
        std::fs::write(dir.join("grows").join("a.bin"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("same").join("b.bin"), vec![1u8; 500]).unwrap();
        let engine = |d: &std::path::Path| {
            let e = Arc::new(Engine::from_indexes(vec![rapidfind_core::folder::build(&d.to_string_lossy(), rapidfind_core::fold::FoldMode::Lower)]));
            e.enable_history(hist.clone());
            e
        };
        let eng = engine(&dir);
        let mut be = rapidfind_core::ipc::Local::new(eng.clone());
        assert!(call(&mut be, "folder_growth", &json!({})).unwrap_err().contains("no snapshots"));
        let mut t = Tree::build(&eng, "", SearchFlags::default()).unwrap();
        let mut s = t.snapshot(&eng, "before", 0, 100).unwrap();
        s.time -= 3 * 86_400;
        eng.history().unwrap().save(&s).unwrap();
        std::fs::write(dir.join("grows").join("c.bin"), vec![1u8; 4000]).unwrap();
        let eng = engine(&dir);
        let mut be = rapidfind_core::ipc::Local::new(eng);
        let v = call(&mut be, "folder_growth", &json!({ "list_snapshots": true })).unwrap();
        assert_eq!(v["snapshots"][0]["save_point"], json!("before"), "{v}");
        let p = dir.to_string_lossy().to_string();
        for args in [json!({ "path": p, "days": 3 }), json!({ "path": p, "since": "before" })] {
            let v = call(&mut be, "folder_growth", &args).unwrap();
            assert_eq!(v["change"], json!(4000), "{v}");
            let kids = v["children"].as_array().unwrap();
            assert_eq!(kids.len(), 1, "unchanged folders are left out: {v}");
            assert_eq!((kids[0]["name"].as_str(), kids[0]["change"].as_i64()), (Some("grows"), Some(4000)));
            assert_eq!(kids[0]["children"][0]["name"], json!("c.bin"));
        }
        assert!(call(&mut be, "folder_growth", &json!({ "since": "nonsense" })).is_err());
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("2026-10-20"), Some(1_792_454_400));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn protected_paths() {
        for p in [r"C:\", "C:", r"D:\", r"\\nas\media", r"\\nas\media\", r"C:\Windows", r"C:\Windows\System32\drivers", r"C:\Users", r"C:\Users\me", r"C:\pagefile.sys", r"C:\$Recycle.Bin\S-1-5"] {
            assert!(protected(p).is_some(), "{p} should be protected");
        }
        for p in [r"C:\Users\me\Downloads\x.iso", r"C:\Windows\Temp\x.tmp", r"\\nas\media\old", r"D:\Projects\node_modules", r"C:\Users\me\AppData\Local\Temp"] {
            assert!(protected(p).is_none(), "{p} should be deletable: {:?}", protected(p));
        }
    }
}
