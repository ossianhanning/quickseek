# rapidfind user guide
Everything about using rapidfind: the window, the query language, what gets indexed, the command
line, the service and the MCP server. The [README](../README.md) has the overview.
## Contents
- [Installing](#installing)
- [The window](#the-window)
- [Folder growth history](#folder-growth-history)
- [Query syntax](#query-syntax)
- [What gets indexed](#what-gets-indexed)
- [Command line](#command-line)
- [MCP server](#mcp-server-rapidfind-mcp)
- [Updating](#updating)
- [Service security](#service-security)
## Installing
Run **`rapidfind-setup-<version>-x64.exe`** (from `tools\build-release.ps1`,
or once published `winget install OssianHanning.rapidfind`). It installs to
`C:\Program Files\rapidfind`: the background service, a Start menu entry, the
`rapidfind` command on PATH, optionally starting with Windows, and optionally
connects your AI assistants (see the MCP section). Uninstall from Apps &
Features; your index and settings in `%ProgramData%\rapidfind` are kept
(`rapidfind-setup.exe --uninstall --purge` removes them too).

Silent switches (winget passes `--silent`): `--dir <folder>`, `--no-service`,
`--no-shortcut`, `--no-path`, `--no-startup`, `--no-mcp` (don't connect AI
assistants; detected ones are connected by default, except when installing as
SYSTEM), `--mcp-clients cursor,claude-code`, `--log <file>`. Through winget:
`winget install OssianHanning.rapidfind` (add `--custom "--no-mcp"` to skip the assistants).

Reading the MFT needs administrator rights, so the index normally lives in a
background service. The GUI and CLI talk to it as a normal user over
`\\.\pipe\rapidfind`. Without the installer:

```
rapidfind service install     # once, elevated: copies the exe to %ProgramData%\rapidfind, registers + starts it
rapidfind-gui                 # then, as a normal user
```

Without the service, `rapidfind-gui` and `rapidfind` index in-process when
run elevated (or with `--local`). Their indexes then live in
`%LOCALAPPDATA%\rapidfind`.

## Command line

```
rapidfind                      # terminal search-as-you-type
rapidfind search -l -s size -d "ext:mp4 size:>1gb"
rapidfind search --case --path "Users\me"
rapidfind search -n 0 "D:\code\ *.rs content:unsafe"   # files containing text (read as you)
rapidfind status               # service, pipe, volumes (with folder/share states), index files
rapidfind du C:\Users -d 2       # biggest folders/files, 2 levels (no path: all drives)
rapidfind du -q "*.mp4|*.mkv"    # only count videos
rapidfind history growth C:\Users --days 7   # what grew or shrank in the last week
rapidfind history list | save "before cleanup"
rapidfind folders add \\nas\photos --rescan-minutes 60
rapidfind folders list | remove <path>
rapidfind config show | set config.json | exclude-folder node_modules | exclude-files "*.tmp"
rapidfind scan                 # build/refresh a local index (admin)
rapidfind bench "*.dll"        # (admin)
rapidfind selftest -v C        # verify against the real filesystem + live updates (admin)
rapidfind watch -v C           # dump USN journal records as they happen (admin)
rapidfind service update | stop | start | uninstall
```

## The window
| | |
|---|---|
| Search box | Results update as you type. `↓`/`PgDn` moves to the list, `Enter` opens, `Esc` clears (or hides to the tray when the box is empty). |
| Filter dropdown | Everything, Audio, Compressed, Document, Executable, Folder, Picture, Video (the `audio:`, `zip:`, ... macros). |
| Columns | Name, Path, Size (Everything-style `1,234 KB`, with folder sizes), Date Modified, Type (the shell's type name). Click a header to sort; size and date sort descending first. View → Sort by Run Count / Date Created. |
| Results | A virtual list: only visible rows are fetched from the service, so scrolling 6M results is smooth. Matched text is bold. Selection count and query time show in the status bar, plus the state of any volume or share that isn't ready. |
| Context menu | Open, Open Path, Copy Full Path / Name / Path, Properties, followed by the real Explorer menu (7-Zip, Send to, Open with, ...). |
| Deleting | `Del` (Recycle Bin) or `Shift+Del` (permanently), in the list or the tree, or Delete in the context menu (with `Shift` for permanently). It runs on a thread of its own with Windows' usual confirmation and progress dialogs, so the window stays usable however big the folder is. The rows turn grey at once, and the tree takes their size off every folder above them straight away. When it's done the status bar says what happened and the views refresh (open folders and the selection stay). Drives and share roots are never deleted. Space moved to the Recycle Bin is freed when the bin is emptied. |
| Drag and drop | Drag results into Explorer, editors or mail. |
| Search menu | Match Case, Whole Word, Match Path, Match Diacritics, Regex. |
| Tools → Options | **General:** hotkey (default `Ctrl+Alt+Space`), run in background, start with Windows, diacritics. **Volumes:** auto-include fixed/removable volumes, per-volume include, "include only" folders. **Folders:** folders and network shares, rescan interval, watching. **Exclude:** hidden/system, folders (paths or name patterns), file patterns, include-only patterns. **Index:** what the index stores (see below). **Shares:** caching for network shares (see below). |
| Content search | `content:text` (see below) shows the name matches at once, then reads those files on a pool of threads and drops the ones that don't contain the text, top of the list first. A progress bar and "searching contents: 1,234 of 5,678 files, 12 found" show in the status bar; `Esc` (or Search → Stop Content Search) stops, leaving the unread rows listed, and a second `Esc` clears the box. Files over 64 MB, binaries and online-only cloud files are skipped (`content_max_mb`, `content_binary`, `content_ifilter` in `gui.json`). The search isn't redone on every file change; `F5` does. |
| Preview pane | View → Preview Pane, the **Preview** button or `Alt+P` opens it right of the list (drag the gap to resize). Text files show with the `content:` matches highlighted and scrolled to the first one (UTF-8, UTF-16 and ANSI, the first 512 KB). Pictures, videos and folders show the shell's thumbnail. Everything else goes to the preview handler Explorer uses (Office documents, PDFs, fonts, ...), usually in its own `prevhost.exe`. Online-only files aren't downloaded for a preview. |
| Folder size tree | The **Tree** button left of the search box (or View → Folder Size Tree, `Ctrl+T`) swaps the list for an expandable tree of drives and folders: +/− boxes, Size, a bar with each entry's share of its parent (for a drive, of the disk), file counts, online-only bytes and dates. Folders holding a single folder open through to it. With text in the search box, only matching files count, so `*.mp4` shows where videos are and `folder: wfn:node_modules` where node_modules weigh. `→`/`←` expand and collapse (or go to the parent), double-click or `Enter` opens or closes a folder, and the context menu has Search in This Folder plus the Explorer menu (delete etc.; sizes refresh after it). In the result list, "Show in Folder Size Tree" opens the tree at that item. Column headers sort the tree. The tree is live: when the index changes (a download finishing, a delete in Explorer or a script) it adds up the sizes again in the background, at most every 5 seconds, not while you're clicking or scrolling and not while the window is hidden, keeping open folders and the selection. Sizes count what's on the disk: online-only cloud files (OneDrive placeholders) are shown in their own column instead, and a file with several hard links counts once. |
| Folder growth | In the tree, View → Compare with Yesterday / a Week Ago / a Month Ago (or a click on the timeline) compares every size with a snapshot. Each bar keeps a ghost of its old size, with the growth in red and shrinkage in blue; a **Change** column (sorted by, biggest growth first) and a **Trend** sparkline per row appear, and the tree opens down to where most of the growth went. The **timeline** above the tree plots the change of the selected folder's biggest children since the first snapshot, with save points as dots, the compared span shaded and, for a drive, a forecast of when it fills at the last two weeks' pace. The **details pane** on the right shows **Where it went** (a waterfall from the old size, or old free space for a drive, through each child's change to now) and **Growth path** (the chain of folders the growth came down through). **Map** is a squarified treemap of the folder two levels deep, each box as big as the item was at its largest (so what shrank or was deleted still shows), red where it grew and blue where it shrank; click a box to open that folder, right-click to go up. **Calendar** colors each of the last months' days by how much the folder grew or shrank that day (a change between snapshots further apart than a day is spread over the days between, marked with a white corner); click a day to compare with it. View → Take a Save Point stores a named snapshot (e.g. before a cleanup) for a year. `Esc` stops comparing. |
| Theme | View → Theme: **System** (the default) follows Windows' light or dark app mode, and switches when Windows does; **Light** or **Dark** keeps one. Title bar, menus, lists, charts, dialogs and the preview follow it. High contrast always uses the system colours. |
| Shortcuts | `Ctrl+F`/`Ctrl+L` focus search, `Ctrl+C` copy full paths, `Ctrl+Shift+C` copy names, `Ctrl+A` select all, `F5` refresh, `Ctrl+P` options, `Ctrl+T` folder size tree, `Del`/`Shift+Del` delete (Recycle Bin / permanently), `Ctrl+Q` quit. |
| Other | One instance per user (launching again shows the window), tray icon, File → Export Results (CSV), run history (opening a file counts toward `runcount:` and the Run Count sort). |

## Folder growth history
The service takes a snapshot of folder sizes once a day (checked every 30
minutes, so a PC that's off at midnight catches up when it starts), and on
demand as a save point. A snapshot holds every folder of 10 MB or more
(at most 60,000 per drive; the cutoff rises past that) with its size, file
count and online-only bytes, every file of 10 MB or more (such as the page
file), plus each drive's capacity and free space. It is stored as varint
columns in zstd: on a test PC 29,372 folders take 313 KB and 0.26 s. Snapshots live in `%ProgramData%\rapidfind\history` (or
`%LOCALAPPDATA%\rapidfind\history` without the service) and are thinned out
as they age: all from the last two days, then one a day for 31 days, one a
week for 26 weeks and one a month for 24 months, about 80 files or 25 MB in
all. Save points are kept for a year.

Comparing loads the snapshot as sorted (path hash, size) pairs, 16 bytes per
folder, so a comparison of all drives takes about 0.35 s. A file the
snapshot kept is compared with its size then; a smaller file modified since
counts as new, and an unmodified one as unchanged. A folder that was under
10 MB then counts as new. Snapshots from before files were kept can't tell
how much a big file changed: such files are listed as not compared.

## Query syntax
| | |
|---|---|
| `foo bar` | name contains `foo` AND `bar` (case-insensitive) |
| `foo\|bar`, `!foo`, `<a b>\|c` | OR (binds tighter than AND, like Everything), NOT, grouping |
| `"a b"` | literal with spaces |
| `*.rs`, `a?c` | wildcards match the whole name |
| `users\me`, `users/me` | a path separator matches against the full path |
| `users\*\*.pdf`, `C:\**\node_modules`, `photo*\*\*.jpg` | in a path, `*` (or `**`) spans any number of folders, and `\*\` also matches none (`a\*\b` finds `a\b`). The pattern may start anywhere in the path but must match through to its end. |
| `wfn:node_modules` | the whole name exactly (`wfn:C:\Temp` a whole path) |
| `case:` `nocase:` `ww:` `path:` `regex:` | per-term options (the Search menu sets defaults) |
| `ext:jpg;png` | extensions |
| `size:>10mb`, `size:1mb..2mb` | size |
| `dm:today` `dm:lastweek` `dm:2026-01..2026-06` `dm:last3days` `dm:>2026-10-01` | date modified (`today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`, `thisyear`, `lastyear`, `lastN<unit>`, `YYYY[-MM[-DD]]`, ranges, comparisons) |
| `dc:` `rc:` | date created / recently changed (seen in the journal); bare `rc:` = anything changed since indexing |
| `parent:C:\dir` | direct children of a folder |
| `depth:2`, `depth:<3` | path depth below the volume root |
| `empty:` | folders without children |
| `attrib:HS` | attributes (R H S D A C E O L T P I) |
| `dupe:`, `sizedupe:` | names (or file sizes) that occur more than once |
| `runcount:>0` | opened from rapidfind |
| `file:` `folder:` `audio:` `video:` `pic:` `doc:` `exe:` `zip:` | type filters |
| `content:foo`, `content:"two words"` | the file contains the text (case-insensitive). Read by the client as you, never by the service |
| `case:content:Foo` `ww:content:foo` | case-sensitive / whole word (the Search menu's Match Case, Whole Word and Regex apply too) |
| `regex:content:fo+\d`, `content-regex:^#include` | a regular expression (`^` and `$` match at line ends) |
| `!content:foo` | doesn't contain it |

Content terms can only be AND-ed with the rest (`*.log content:error` yes,
`content:a | content:b` or `!<content:a>` no), as in Everything: they filter
what the rest of the query found. Text is decoded first: UTF-8 with or
without a BOM, UTF-16 LE/BE with a BOM (or recognisable by its zero bytes),
otherwise the ANSI code page. A file with other NUL bytes in its first 8 KB
is binary and skipped. Word, Excel, PowerPoint (and PDFs, where a PDF reader
installed a filter) are read through Windows Search's IFilter for their type.
Files are read in 1 MiB chunks with a 64 KiB overlap, and reading stops at
the first match. Over the pipe, the GUI walks the result set in display order
(`Snapshot`) and tells the service which rows failed (`DropRows`), so sorting
and paging keep working on the service side while the list shrinks.

Diacritics: by default `a` and `å` are different. Turn on "ignore
diacritics" (Search → Match Diacritics off) to make `Angstrom` find
`Ångström`.

## What gets indexed
- **NTFS volumes.** Fixed volumes are included automatically (removable ones
  on request). They're read from the MFT and kept live from the USN journal.
- **ReFS volumes** (e.g. Dev Drives) are enumerated with `FSCTL_ENUM_USN_DATA`
  (V3, 128-bit file ids) and kept live from the journal. Sizes aren't
  available that way.
- **Folders** (FAT/exFAT drives, network shares, anything else) are walked in
  parallel and watched with `ReadDirectoryChangesW`, with optional periodic
  rescans. A watch buffer overflow triggers a rescan. Folder indexes are
  cached on disk, so a restart serves the last copy while the folder is
  re-walked in the background.
- **Network shares.** Add `\\server\share\path` (mapped drives are converted
  to UNC, since drive mappings only exist in your logon session). The
  service runs as LocalSystem, which has no access to your shares, so it
  scans them **with the identity of the user whose GUI/CLI connects to it**
  (impersonating the pipe client). Until someone connects after a reboot,
  shares show "waiting for credentials" and serve the cached index. An
  unreachable share shows "offline", keeps its last index and is retried.
  Shares added in the GUI default to watching plus an hourly rescan, in case
  change notifications get lost over a WAN link.
- **Drives coming and going** are noticed within a few seconds (the service
  polls the drive set). Removed drives drop out of results.
- **Excludes** (Options → Exclude, or `config.json`) are applied as flags,
  not by re-indexing, so changing them takes effect immediately.

### Index options (Options → Index, or `rapidfind config index-created|fast-sort`)

Names, parents, sizes, dates modified and attributes are always kept. The
rest is optional:

| Option | Default | Cost | When off |
|---|---|---|---|
| Index date created | off | 8 B/file in memory (~55 MB for 7M files) plus one more compressed column on disk | `dc:` and sorting by Date Created report that it isn't indexed. Turning it on rescans each volume once (C: ~30 s); the old index keeps serving meanwhile. |
| Fast size sort | on | 4 B/file | First page of a multi-million-hit size sort takes ~150 ms instead of ~70 ms; normal result sets are unaffected. |
| Fast date modified sort | on | 4 B/file | Same as size (~120 ms instead of ~40 ms at 3.3M hits). |
| Fast path sort | on | 4 B/file | Path sorts compare full paths: ~0.75 s for 250k hits, several seconds for millions. |

Turning a fast sort back on computes its ranks in the background (a second
or two per volume); nothing is rescanned.

### Network shares (Options → Shares)

Walking a share over a slow link can take minutes to hours, so shares have
their own settings:

| Option | Default | Trade-off |
|---|---|---|
| Keep a copy of share indexes on disk | on | About 12 bytes per file on disk. Off: after a restart a share is empty until it has been walked again. |
| Walk shares again when the service starts | on | Off: serve the cached copy and only watch for changes (ready in ~50 ms instead of a full walk). Changes made while the service was stopped show up at the next rescan. |
| Index date created on shares | on | 8 B/file in memory, no extra traffic (it comes with the listing). Separate from the local switch because turning it on later re-walks every share. |
| Rescan newly added shares every | 60 min | A periodic walk catches change notifications lost over SMB. |

Configuration is `config.json` next to the index: `%ProgramData%\rapidfind`
for the service, `%LOCALAPPDATA%\rapidfind` in-process. GUI preferences are
in `%APPDATA%\rapidfind\gui.json`.


## MCP server (rapidfind-mcp)
`rapidfind-mcp.exe` is a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio. It talks to the service like the GUI does, so it needs no
elevation, and anything it deletes, it deletes with the rights of the user who
started it. Registering it is per user and needs no admin, three ways:

- **GUI:** Tools → Connect AI Assistants… (a checkbox per client).
- **CLI:** `rapidfind mcp install` (every detected client, or `--client cursor`),
  `rapidfind mcp status`, `rapidfind mcp uninstall`.
- **Installer:** the "Connect AI assistants" option, on by default, also for winget (`--no-mcp` skips it).

Supported clients: Claude Code (through `claude mcp add --scope user`), Claude
Desktop, Cursor, VS Code and VS Code Insiders (agent mode), Windsurf,
Antigravity (`~/.gemini/config/mcp_config.json`) and Codex
(`[mcp_servers.rapidfind]` in `~/.codex/config.toml`). Their configs are
merged, not replaced. Other servers and the key order are kept (Codex's TOML
keeps its comments and layout too), a `.rapidfind-backup` copy is written
first, and a JSON file with comments is left alone (you get the entry to add by
hand). Claude Code and Codex also get the rapidfind skill (`~/.claude/skills`,
`~/.agents/skills`), which `rapidfind-mcp` keeps up to date after updates. When `rapidfind-mcp.exe` isn't
in a stable place like Program Files, it's copied to
`%LOCALAPPDATA%\Programs\rapidfind` first, so a rebuild or `cargo clean`
doesn't break the clients. Restart Claude Desktop afterwards; Claude Code picks
it up in new sessions.

For Claude Desktop there's also **`rapidfind-<version>.mcpb`**, a Desktop
Extension built by `tools\build-release.ps1` (or `tools\build-mcpb.ps1`):
double-click it to install.

| Tool | |
|---|---|
| `search_files` | The full query language (wildcards across folders, `ext:`, `size:`, `dm:`, `dupe:` ...) plus structured filters (`name`, `path`, `extensions`, `min_size`/`max_size`, `modified_after`/`before`, `type`), sorting and paging. Online-only cloud files are flagged. `content` (or `content:` in `query`, `content_regex` for a regex) keeps files containing the text: it reads at most 256 MB per call (files over 32 MB are skipped) and stops once it has a page of matches; the reply's `content` part says how many files were read and whether the search was complete. |
| `folder_sizes` | Disk-usage breakdown of a path (or of all drives, with capacity and free space), `depth` levels, top `limit` per folder with the rest summarised. With `query`, only matches count. |
| `folder_growth` | What grew or shrank under a path since a snapshot `days` back (default 7) or `since` a date or save point, biggest change first, `depth` levels following the changes, with sizes and drives' free space then and now. `list_snapshots` lists the snapshots. |
| `disk_overview` | Drives and shares: capacity, free, indexed size, online-only bytes, file count, index state. |
| `cleanup_candidates` | Reclaimable space by category: Recycle Bin, temp folders, node_modules and other build output, Rust `target` folders, package manager caches, crash dumps, previous Windows installations, big old downloads, huge logs and same-size duplicates. Each category comes with a total and its biggest items. Read-only. |
| `delete_paths` | Moves to the Recycle Bin by default; `permanent` deletes outright (required on shares); `dry_run` reports sizes only. Refuses volume and share roots, the Windows folder, Program Files, profile roots and system files. |
| `empty_recycle_bin` | Reports and (unless `dry_run`) empties the Recycle Bin of one drive or all drives. |

On a test PC with 6.8M files, `cleanup_candidates` answers in 1.6 s,
`folder_sizes` in about 0.3 s and `search_files` in 10 to 40 ms.

## Updating

- Running the new `rapidfind-setup.exe` replaces the programs in the install
  folder (moving running ones aside), updates the service's copy and restarts
  it, then ends `rapidfind-mcp` processes still running the old code. AI apps
  start the new one on their next call.
- `rapidfind service install` / `update` from anywhere else (a development
  build in `target\release`) also copies `rapidfind.exe`, `rapidfind-gui.exe`
  and `rapidfind-mcp.exe` from next to it into the install folder, so the
  service and its clients stay the same build.
- Clients and the service agree on a protocol version when they connect; the
  service also serves the previous one, so a GUI or MCP server one release
  behind keeps working. When they share none, the MCP server answers with a
  clear error and, if its file was replaced since it started, exits so the
  app restarts it. `rapidfind status` shows both builds; `service.log` logs
  the service's build at startup and clients of other builds as they connect.
- If the index can't be saved (the disk holding `%ProgramData%` is full), the
  volume's status says so (GUI status bar, `rapidfind status`, the MCP
  server's `disk_overview` and errors). Searches keep working, and saves wait
  until there's room again.

## Service security

- The binary the service runs, its index, config and logs live in
  `%ProgramData%\rapidfind`, created with a protected DACL (SYSTEM +
  Administrators only). Users can't replace what runs as SYSTEM or read the
  index files.
- Pipe DACL: SYSTEM, Administrators and the owner get full access. Interactive
  users can only read and write data: no `FILE_CREATE_PIPE_INSTANCE`, so they
  can't add server instances. Remote clients are rejected
  (`PIPE_REJECT_REMOTE_CLIENTS`), and the pipe is created with
  `FILE_FLAG_FIRST_PIPE_INSTANCE`.
- Any interactive user can search every indexed name, including other
  profiles' file names, and can change the index configuration through the
  pipe. Everything's service works the same way. Don't install the service on
  shared machines if that matters.
- For network folders, the service keeps an impersonation token of the last
  connected user and only uses it to read the configured folders.
- Content search never runs in the service: the GUI, CLI or MCP server reads
  the files itself, as the user running it, so it only finds text in files
  that user may read. Index-wide name search still covers everything.


