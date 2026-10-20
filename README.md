# rapidfind

Instant filename search for Windows, in the style of voidtools' Everything,
written in Rust. It indexes NTFS volumes from the MFT, ReFS volumes through
the USN journal, and any other folder or network share by walking it, then
keeps everything live.

- **`rapidfind-gui`**: an Everything-style window: search-as-you-type,
  a virtual results list over millions of rows, sortable columns, a type
  filter, highlighted matches, the Explorer context menu, drag-out, a tray
  icon and a global hotkey.
- **`rapidfind`**: the CLI, terminal UI and Windows service.

## Getting started

Reading the MFT needs administrator rights, so the index normally lives in a
background service. The GUI and CLI talk to it as a normal user over
`\\.\pipe\rapidfind`.

```
rapidfind service install     # once, elevated: copies the exe to %ProgramData%\rapidfind, registers + starts it
rapidfind-gui                 # then, as a normal user
```

Without the service, `rapidfind-gui` and `rapidfind` index in-process when
run elevated (or with `--local`). Their indexes then live in
`%LOCALAPPDATA%\rapidfind`.

```
rapidfind                      # terminal search-as-you-type
rapidfind search -l -s size -d "ext:mp4 size:>1gb"
rapidfind search --case --path "Users\me"
rapidfind status               # service, pipe, volumes (with folder/share states), index files
rapidfind folders add \\nas\photos --rescan-minutes 60
rapidfind folders list | remove <path>
rapidfind config show | set config.json | exclude-folder node_modules | exclude-files "*.tmp"
rapidfind scan                 # build/refresh a local index (admin)
rapidfind bench "*.dll"        # (admin)
rapidfind selftest -v C        # verify against the real filesystem + live updates (admin)
rapidfind watch -v C           # dump USN journal records as they happen (admin)
rapidfind service update | stop | start | uninstall
```

## The GUI

| | |
|---|---|
| Search box | Results update as you type. `↓`/`PgDn` moves to the list, `Enter` opens, `Esc` clears (or hides to the tray when the box is empty). |
| Filter dropdown | Everything, Audio, Compressed, Document, Executable, Folder, Picture, Video (the `audio:`, `zip:`, ... macros). |
| Columns | Name, Path, Size (Everything-style `1,234 KB`, with folder sizes), Date Modified, Type (the shell's type name). Click a header to sort; size and date sort descending first. View → Sort by Run Count / Date Created. |
| Results | A virtual list: only visible rows are fetched from the service, so scrolling 6M results is smooth. Matched text is bold. Selection count and query time show in the status bar, plus the state of any volume or share that isn't ready. |
| Context menu | Open, Open Path, Copy Full Path / Name / Path, Properties, followed by the real Explorer menu (7-Zip, Send to, Open with, ...). |
| Drag and drop | Drag results into Explorer, editors or mail. |
| Search menu | Match Case, Whole Word, Match Path, Match Diacritics, Regex. |
| Tools → Options | **General:** hotkey (default `Ctrl+Alt+Space`), run in background, start with Windows, diacritics. **Volumes:** auto-include fixed/removable volumes, per-volume include, "include only" folders. **Folders:** folders and network shares, rescan interval, watching. **Exclude:** hidden/system, folders (paths or name patterns), file patterns, include-only patterns. |
| Shortcuts | `Ctrl+F`/`Ctrl+L` focus search, `Ctrl+C` copy full paths, `Ctrl+Shift+C` copy names, `Ctrl+A` select all, `F5` refresh, `Ctrl+P` options, `Ctrl+Q` quit. |
| Other | One instance per user (launching again shows the window), tray icon, File → Export Results (CSV), run history (opening a file counts toward `runcount:` and the Run Count sort). |

## Query syntax

| | |
|---|---|
| `foo bar` | name contains `foo` AND `bar` (case-insensitive) |
| `foo\|bar`, `!foo`, `<a b>\|c` | OR (binds tighter than AND, like Everything), NOT, grouping |
| `"a b"` | literal with spaces |
| `*.rs`, `a?c` | wildcards match the whole name |
| `users\me`, `users/me` | a path separator matches against the full path |
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

Configuration is `config.json` next to the index: `%ProgramData%\rapidfind`
for the service, `%LOCALAPPDATA%\rapidfind` in-process. GUI preferences are
in `%APPDATA%\rapidfind\gui.json`.

### Service security

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

## How it is fast

| Phase | Technique |
|---|---|
| Initial index | Read `$MFT` straight off the volume device (`\\.\C:`) instead of walking directories. The run list is decoded from MFT record 0, following `$ATTRIBUTE_LIST` when the MFT itself is fragmented. |
| | `$MFT::$BITMAP` skips unused records; chunks with no live records are never read, and within a chunk only spans of in-use records are read. |
| | 4 MiB unbuffered (`FILE_FLAG_NO_BUFFERING`) positional reads into page-aligned buffers, issued by every rayon worker at once (many reads in flight). Each worker parses its own chunk in place (fixups, attribute walk, UTF-16 → UTF-8). |
| | If a volume filter driver rejects raw volume reads (e.g. `gameflt` on Xbox/Store game drives), the same bytes are read from `\\.\PhysicalDriveN` at the volume's disk offset. |
| | Raw records give names, parent, size and timestamps in one pass. `FSCTL_ENUM_USN_DATA` is the fallback (no sizes or dates). |
| Staying current | `FSCTL_READ_USN_JOURNAL` with `BytesToWaitFor = 1` blocks in the kernel until something changes. Records are applied through an MFT-record → entry map, so there are no per-change path lookups. Only files whose data changed get a batched `OpenFileById` size refresh. Directory renames keep their entry, so children follow for free. |
| Restart | The index is saved column by column (names, parents as distance to the entry, records and dates as deltas, all varints) in 4 MiB zstd blocks compressed and decompressed in parallel, plus the journal id and next USN. On startup only the journal records since the last run are replayed. Derived data (folded names, record map, sort ranks) isn't stored: it's rebuilt in parallel, the ranks in the background. |
| Compact memory | 40-byte entries with attributes packed into the flag bits. A name is stored once when it equals its folded form (most names), and fold ownership is a bitset instead of a per-entry column. |
| Search | All folded names live in one contiguous `\0`-separated byte arena. A substring query is a parallel SIMD `memmem` over it, and matches map back to entries by galloping search. Exclusions are a cache-resident bitset. |
| Sorting | Per-volume ranks for name, size, date and path (Everything's "fast sort"). The first page is a top-k selection over `(rank << 32 \| index)` integers gathered in parallel. Path order is a depth-first walk with siblings in name order. Entries changed since ranking fall back to exact comparisons that agree with the ranks, and ranks are refreshed in the background. |
| Paging | A result set is kept per connection. The GUI's virtual list fetches 256-row blocks as you scroll, and the full sort only happens if you scroll past the first page. |

## Measured (this machine, 6.8M files on C:, F: and S:)

| | |
|---|---|
| Full MFT scan | S: (NVMe, 1.45M records) ~2.6 s · F: (NVMe, via physical disk) 2.7 s · C: (SATA QLC, 4.4M records, 3.7 GiB MFT) ~25 s, bound by the drive |
| Restart (load + journal replay) | ~0.5–1.5 s per volume; sort ranks follow in the background |
| Live update visible | 1–10 ms after the change (NTFS journal); immediately for folders and SMB shares (change notifications) |
| Queries, all volumes | `txt` 20 ms · `*.dll` 46 ms · `dm:thisweek` 24 ms · `pic: size:>5mb` 26 ms · `a` (4.3M hits) 200 ms · empty query (6.8M) 280 ms |
| Index on disk | 84 MB for all three volumes (C: 40 MB for 3.27M entries); Everything's db for the same drives is 260 MB. Saving C: takes ~0.5 s. |
| Memory | Service 676 MB private for 6.8M entries (Everything: ~560 MB). C: is 319 MB: entries 127, names 35, folded names 75, maps 31, sort ranks 51. |
| Correctness | `selftest`: 3,002 random entries checked against the filesystem: 0 missing, 0 size mismatches |

## Testing

```
cargo test --workspace --exclude rapidfind-gui          # core + CLI unit tests
cargo test -p rapidfind-gui --bins                      # GUI logic (highlighting, paging cache, settings)
cargo test -p rapidfind-gui --test ui -- --test-threads=1
```

The last command is the GUI end-to-end test. It starts the real
`rapidfind-gui.exe` against a mock service (synthetic index, private pipe)
and drives it through UI Automation and MSAA:

- typing a query filters results, with correct Name/Path/Size/Type cells
- header clicks sort by size, both directions
- deep paging to the last row
- the filter dropdown
- Match Case and Match Diacritics
- live updates
- the context menu's Copy Full Path, checked on the clipboard
- Options → Exclude adding a pattern the service applies
- `Esc` clears the search

It opens a window and clicks, so don't type while it runs.

Also:

- `$env:RAPIDFIND_TEST_UNC='\\localhost\S\some\dir'; cargo test -p rapidfind-core --release unc_share -- --ignored` indexes and watches a real SMB share.
- `cargo run -p rapidfind-gui --example mock_service -- --live` serves a synthetic index. Start the GUI with `RAPIDFIND_PIPE=\\.\pipe\rapidfind-mock` to try it without the service.
- `rapidfind selftest` (elevated) checks the real index against the filesystem and exercises live updates.

## Layout

- `crates/core`: the library
  - `ntfs/`: `mft.rs` (raw scan), `record.rs` (FILE record parsing), `usn.rs` (records, enum fallback, ReFS), `journal.rs`, `win.rs` (volumes, tokens, UNC)
  - `index.rs` (entries, folded arena, ranks, excludes), `query.rs`, `date.rs`, `search.rs` (result sets), `live.rs` (journal → index), `folder.rs` (walker + watcher), `config.rs`, `runs.rs`, `persist.rs`, `engine.rs`, `ipc.rs` (protocol, sessions, server, client, backends)
- `crates/cli`: the `rapidfind` binary: CLI, terminal UI and `service.rs` (the Windows service)
- `crates/gui`: `rapidfind-gui`: `app.rs` (main window), `options.rs`, `shell.rs` (icons, context menu, drag-out, dialogs), `highlight.rs`, `results.rs`, `settings.rs`; `tests/ui.rs` (end-to-end), `examples/mock_service.rs`

## Prior art

Studied while writing the NTFS layer:
[ntfs-reader](https://crates.io/crates/ntfs-reader) (MIT/Apache-2.0) and
[Indexer++](https://github.com/dfs-minded/indexer-plus-plus) (MIT), plus
voidtools' public notes on Everything (reads the MFT for the initial index and
uses the USN journal for updates).

## Not yet

- Content search, a preview pane, ETP/HTTP servers and EFU file lists.
- ReFS volumes have no sizes yet, and ReFS hasn't been tested on a real volume (only with synthetic journal records).
- Removing one hard link of a multi-link file isn't tracked precisely.
