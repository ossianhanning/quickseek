//! How assistants learn when to use rapidfind: the server instructions
//! (Claude Code puts them in its system prompt), MCP prompts (slash commands
//! / attach-menu entries) and a Claude Code skill that loads itself when a
//! request is about finding files, file contents or disk space on this PC.

use serde_json::{Value, json};

/// The `instructions` of the `initialize` reply: when to reach for these
/// tools, phrased like the requests that should trigger them.
pub const INSTRUCTIONS: &str = "rapidfind has an always-current index of every file and folder on this Windows PC's drives (and configured network shares) \
with names, paths, sizes and dates, so it answers instantly without scanning the disk. \
Use these tools WHENEVER a request involves files on this computer, instead of shell commands (dir, Get-ChildItem, where, findstr, \
Select-String over folders, du, scripts) or the Glob/Grep tools outside the current project, which walk the disk slowly. For example: \
\"where is my tax return pdf\", \"find the file called ...\", \"find all .psd files\", \"files I changed today\", \"which files mention X\" \
(content search), \"find duplicates\", \"largest files\", \"what's in my Downloads\", \"what has eaten my disk on D:\", \"free up space on C:\", \
\"my drive is full\", \"why is my disk filling up\", \"what grew this week\", \"delete node_modules / temp files\". \
Finding: search_files (name, path wildcards like users\\*\\*.pdf where * spans folders, ext, size, dates, dupes, `content` for text inside files). \
Disk space: disk_overview, then folder_sizes (path like \"D:\\\", depth 1-2, drill into the biggest folders) and cleanup_candidates; \
what grew or shrank since yesterday, last week, a date or a save point: folder_growth (daily snapshots). \
Before delete_paths, show what would go with sizes and get a yes; it recycles by default, so space is freed only after \
empty_recycle_bin (ask first) or permanent: true. Online-only OneDrive files are reported apart (they take no disk space). \
Results that read better as a picture carry an optional `chart` hint (chart type, fields to plot); draw it if you can render charts.";

/// Name of the Claude Code skill folder (`~/.claude/skills/<name>/SKILL.md`).
pub const SKILL_NAME: &str = "rapidfind";

/// The skill: its description is what Claude matches requests against.
pub const SKILL_MD: &str = r#"---
name: rapidfind
description: Find files and folders on this Windows PC and work out what uses disk space, instantly, with the rapidfind MCP tools (a live index of every file on all drives and network shares). Use whenever a request involves files on this computer outside the current project, for example finding a file by name, part of a name, extension, folder or wildcard path; files changed recently or not for a long time; the largest, oldest or duplicate files; files containing some text; what is in a folder; what is eating or filling a drive (C:, D:, ...); a full disk; what grew or changed in size recently or since a date; cleaning up or freeing space; deleting node_modules, build output, caches or temp files.
---

# rapidfind: files and disk space on this PC

rapidfind keeps a live index of every file on this PC's drives (and indexed network shares) with
names, paths, sizes and dates. Its MCP tools (`mcp__rapidfind__*`) answer in milliseconds. Use
them instead of `dir`, `Get-ChildItem -Recurse`, `where`, `findstr`, `du` or Glob/Grep over whole
drives, which crawl the disk.

## Finding files: `search_files`

- By name: `name` (part of the name, or a wildcard like `report*.docx`), or `query` in
  Everything-like syntax: `invoice 2025 ext:pdf`, `*.psd`, `"exact phrase"`, `a|b`, `!tmp`.
- By location: `path` (`C:\Users\me\Documents`), or a path wildcard in `query` where `*` spans
  any number of folders: `users\*\Downloads\*.iso`, `D:\**\node_modules`.
- By size or date: `min_size` / `max_size` ("500mb"), `modified_after` / `modified_before`
  ("2026-01-01", "last7days", "today"); in `query`: `size:>1gb`, `dm:thisweek`, `dm:<last365days`.
- Inside files: `content` ("some text"; `content_regex` for a pattern), best combined with other
  filters (`extensions`, `path`) since it reads the files.
- More: `dupe:` (same name), `sizedupe:` (same size), `wfn:name` (exact name), `folder:` /
  `file:`, `parent:C:\dir` (direct children), `empty:` (empty folders), `rc:today` (changed today).
- Biggest first: `sort: "size"`; newest first: `sort: "modified"`. `limit` up to 1000, `offset` pages.

Show paths as links or a short table, and say how many matched in total.

## What is using space: `disk_overview`, `folder_sizes`

1. `disk_overview`: capacity and free space per drive.
2. `folder_sizes` with `path` `D:\` and `depth` 2: the biggest folders and files with their share
   of the parent; drill into the largest with further calls. `query` restricts it, e.g. `*.mp4`
   shows where videos take space, `folder: wfn:node_modules` where node_modules folders are.
3. Summarise the biggest items in plain words: size, what they probably are.

## What grew: `folder_growth`

For "why is my disk filling up", "what got bigger this week", "where did 20 GB go", "did the
cleanup help": `folder_growth` compares folder sizes now with a daily snapshot (`days`, default 7)
or a save point / date (`since`; `list_snapshots: true` lists them). It lists what grew or shrank
most under `path`, biggest change first, with sizes then and now and each drive's free space then
and now; `depth` 2-3 follows the biggest changes down. Snapshots keep folders of 10 MB or more, so
a smaller folder then shows as new.

## Freeing space: `cleanup_candidates`, then `delete_paths`

1. `cleanup_candidates` (optionally `path` `C:\`): Recycle Bin, temp folders, node_modules and
   other build output, Rust `target` folders, package caches, crash dumps, old Windows installs,
   big old downloads, huge logs, same-size duplicates.
2. Add big personal folders from `folder_sizes` that the categories don't cover.
3. Propose a list with sizes, grouped by how safe it is. Ask before deleting anything.
4. `delete_paths` with what the user approved (`dry_run: true` to preview). It moves to the
   Recycle Bin by default: say so, and offer `empty_recycle_bin` (ask first) to free the space.

## Good to know

- Sizes are file lengths. Online-only OneDrive/cloud files take no local space and are shown
  separately (`online_only`); a file with several hard links counts once.
- The tools refuse to delete volume roots, Windows, Program Files, profile roots and system
  files like pagefile.sys.
- If the service isn't reachable, the user needs the rapidfind installer or
  `rapidfind service install` (elevated).
"#;

pub fn prompts_list() -> Value {
    json!({ "prompts": [
        {
            "name": "free_disk_space",
            "title": "Free up disk space",
            "description": "Find what can be cleaned up on a drive and free space (asks before deleting anything).",
            "arguments": [{ "name": "drive", "description": "Drive to clean up, e.g. C: (default: the fullest drive)", "required": false }]
        },
        {
            "name": "whats_using_space",
            "title": "What's using space?",
            "description": "Break down which folders and files take the most space on a drive or folder.",
            "arguments": [{ "name": "path", "description": "Drive or folder, e.g. D: or C:\\Users (default: all drives)", "required": false }]
        },
        {
            "name": "whats_growing",
            "title": "What's growing?",
            "description": "Show which folders grew or shrank most over the last days (or since a date or save point).",
            "arguments": [
                { "name": "path", "description": "Drive or folder, e.g. C: (default: all drives)", "required": false },
                { "name": "days", "description": "How many days back (default 7)", "required": false }
            ]
        },
        {
            "name": "find_files",
            "title": "Find files",
            "description": "Find files on this PC by a description (name, type, place, size, date or contents).",
            "arguments": [{ "name": "what", "description": "What you're looking for, e.g. \"the invoice from Acme last spring\"", "required": true }]
        },
        {
            "name": "find_large_files",
            "title": "Find large files",
            "description": "List the biggest files (optionally only old ones) so you can decide what to remove.",
            "arguments": [
                { "name": "path", "description": "Where to look (default: everywhere)", "required": false },
                { "name": "older_than_days", "description": "Only files not modified for this many days", "required": false }
            ]
        }
    ]})
}

pub fn prompt_get(name: &str, args: &Value) -> Result<Value, String> {
    let arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let (description, text) = match name {
        "free_disk_space" => {
            let drive = arg("drive")
                .map(|d| format!("{}\\", d.trim_end_matches('\\')))
                .unwrap_or_else(|| "the fullest drive".into());
            (
                "Free up disk space",
                format!(
                    "Help me free up disk space on {drive} using the rapidfind tools. Start with disk_overview and cleanup_candidates, then \
folder_sizes (depth 2) for big folders the categories miss. Show me a list of what could go, with sizes and how safe each is, and wait \
for my OK before deleting anything. After deleting, tell me how much is in the Recycle Bin and ask before emptying it."
                ),
            )
        }
        "whats_using_space" => {
            let path = arg("path")
                .map(String::from)
                .unwrap_or_else(|| "my drives".into());
            (
                "What's using space?",
                format!(
                    "What is using the space on {path}? Use rapidfind's folder_sizes (start at depth 2, then drill into the biggest folders) and summarise the biggest items, what they probably are, and which could be cleaned up."
                ),
            )
        }
        "whats_growing" => {
            let path = arg("path").map(|p| format!(" on {p}")).unwrap_or_default();
            let days = arg("days").unwrap_or("7");
            (
                "What's growing?",
                format!(
                    "What has grown or shrunk{path} over the last {days} days? Use rapidfind's folder_growth (depth 3) and explain the biggest changes in plain words: what the folders probably are, whether the growth looks normal, and what could be cleaned up."
                ),
            )
        }
        "find_files" => {
            let what = arg("what").ok_or("say what to look for")?;
            (
                "Find files",
                format!(
                    "Find this on my computer with rapidfind's search_files: {what}. Try a few searches (name parts, likely extensions and folders, dates, and content search if the name doesn't help), then list the best matches with their paths, sizes and dates."
                ),
            )
        }
        "find_large_files" => {
            let mut t = String::from("Find the largest files");
            if let Some(p) = arg("path") {
                t.push_str(&format!(" in {p}"));
            }
            if let Some(d) = arg("older_than_days") {
                t.push_str(&format!(" not modified in the last {d} days"));
            }
            t.push_str(" with rapidfind's search_files (sort by size, descending, limit 30), and group them by what they are. Don't delete anything.");
            ("Find large files", t)
        }
        _ => return Err(format!("unknown prompt: {name}")),
    };
    Ok(
        json!({ "description": description, "messages": [{ "role": "user", "content": { "type": "text", "text": text } }] }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_is_well_formed() {
        let body = SKILL_MD.strip_prefix("---\n").expect("frontmatter first");
        let (front, rest) = body.split_once("\n---\n").expect("closed frontmatter");
        assert!(front.lines().any(|l| l == format!("name: {SKILL_NAME}")));
        let desc = front
            .lines()
            .find_map(|l| l.strip_prefix("description: "))
            .unwrap();
        assert!(
            desc.len() <= 1024,
            "skill descriptions are capped at 1024 chars ({})",
            desc.len()
        );
        for word in ["find", "disk", "duplicate", "containing"] {
            assert!(desc.contains(word), "{word}");
        }
        for tool in [
            "search_files",
            "folder_sizes",
            "folder_growth",
            "disk_overview",
            "cleanup_candidates",
            "delete_paths",
            "empty_recycle_bin",
        ] {
            assert!(
                rest.contains(tool) && INSTRUCTIONS.contains(&tool[..6]),
                "{tool}"
            );
        }
    }
}
