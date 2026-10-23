# How rapidfind works
Design notes and measurements for contributors. See also [benchmarks](benchmarks.md) against Everything.

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
| Compact memory | 24-byte entries with attributes packed into the flag bits and the date modified as 4-byte seconds (1950-2086). Entries are kept in MFT record order, so a 64-record block index replaces a per-record lookup table and the record number itself isn't stored. A name is stored once when it equals its folded form; a name that differs only in ASCII upper case (most of the rest) is kept as one bit per byte over the folded form, not as a second copy. Fold ownership is a bitset, and name boundaries are found from a sample every 64 names instead of a per-entry offset table. Sort ranks and the name order are bit-packed to the width the volume needs (22 bits for 3M files). |
| Search | All folded names live in one contiguous `\0`-separated byte arena. A substring query is a parallel SIMD `memmem` over it, and matches map back to entries by galloping search; `ext:dll` searches for `.dll\0`, so only real extensions are candidates. Exclusions are a cache-resident bitset. Scans of every entry fill fixed chunks and copy once. A path term starting with a drive (`f:\`) skips the other volumes. |
| Sorting | Per-volume ranks for name, size, date and path (Everything's "fast sort"), plus the name order itself. A name-sorted page with many hits is read off that order: the hits go into a bitset and the order is walked until the page is full, so millions of hits cost a bit test per position rather than a sort. Smaller sets are a top-k selection over `(rank << 32 \| index)` integers gathered in parallel. Path order is a depth-first walk with siblings in name order. Entries changed since ranking are listed, sorted with exact comparisons that agree with the ranks and merged in, and ranks are refreshed in the background. |
| Path wildcards | A path pattern runs as a bit-parallel automaton (shift-and over at most 62 positions). The state after each folder's path is cached, so an entry only feeds its own name, and a folder that can't lead to a match prunes everything below it. A literal ending such as `.pdf` drives the name scan. |
| Folder sizes | Per client tree: child lists (CSR) built with parallel atomic passes, then breadth-first levels whose totals are folded up level by level in parallel (~0.3 s for 6.8M entries, ~12 bytes per entry while open). With a query, matches are added up their ancestor chains in parallel. A matched folder counts with its whole contents and is never counted twice. |
| Paging | A result set is kept per connection. The GUI's virtual list fetches 256-row blocks as you scroll, and the full sort only happens if you scroll past the first page. |

## Measured (one PC, 6.8M files on three SSDs)
| | |
|---|---|
| Full MFT scan | S: (NVMe, 1.45M records) ~2.6 s · F: (NVMe, via physical disk) 2.7 s · C: (SATA QLC, 4.4M records, 3.7 GiB MFT) ~25 s, bound by the drive |
| Restart (load + journal replay) | ~0.5–1.5 s per volume; sort ranks follow in the background |
| Live update visible | 1–10 ms after the change (NTFS journal); immediately for folders and SMB shares (change notifications) |
| Path wildcards | `users\*\*.pdf` 34 ms · `*\appdata\*\*.log` 34 ms · `C:\**\node_modules` 33 ms · `photo*\*\*.jpg` 23 ms · `windows\system32\` 96 ms |
| Folder size tree | built in 0.2 to 0.4 s for all three volumes; expanding a folder takes 10 to 30 ms |
| Queries, all volumes | `txt` 20 ms · `*.dll` 46 ms · `dm:thisweek` 24 ms · `pic: size:>5mb` 26 ms · `a` (4.3M hits) 200 ms · empty query (6.8M) 280 ms |
| Index on disk | Format v6: C: 34.5 MB for 3.27M entries (39.8 MB as v5); 84 MB for all three volumes before v6. Everything's db for the same drives is 260 MB. Saving C: takes ~0.3-0.5 s. |
| Memory | C: from a saved index, ready: 217 MB private (main before this round: 252 MB): entries 81, names 10, folded names 74, maps 1, sort ranks and name order 43. Two copies of C: (6.5M entries) 441 MB; Everything uses 582 MB for 6.44M. |
| Content search | Folder indexes of real trees, in-process (`content_bench` example, release build; first run reads from disk, later runs from the file cache). this repository, `*.rs content:unsafe`: 174 files in 0.13 s, then 0.01–0.02 s. a 840k-entry source drive `*.cpp\|*.h content:TODO`: 16,543 files (273 MB) in 7.7 s cold, 1.0 s warm (16k files/s). PDFs in a Downloads folder, `ext:pdf` with a common word: 306 PDFs through Windows' PDF filter in 10–14 s (text extraction, ~30 files/s; 184 match). Worst case, every file on that drive (`case:content:vkCreateDevice`, 695k files read, 363k of them binary and skipped after 64 KB): 6.5 min, 46 GB at ~1,700 files/s, bound by the disk. Narrow the name query first. |
| Correctness | `selftest`: 3,002 random entries checked against the filesystem: 0 missing, 0 size mismatches |

## Layout
- `crates/core`: the library
  - `ntfs/`: `mft.rs` (raw scan), `record.rs` (FILE record parsing), `usn.rs` (records, enum fallback, ReFS), `journal.rs`, `win.rs` (volumes, tokens, UNC)
  - `index.rs` (entries, folded arena, ranks, excludes), `query.rs`, `date.rs`, `search.rs` (result sets), `content.rs` (content search: query split, decoding, the scan), `ifilter.rs`, `live.rs` (journal → index), `folder.rs` (walker + watcher), `config.rs`, `runs.rs`, `persist.rs`, `engine.rs`, `tree.rs` (folder-size trees), `history.rs` (snapshots, retention), `ipc.rs` (protocol and versions, sessions, server, client, backends), `upgrade.rs` (replacing running programs, retiring old MCP servers)
- `crates/cli`: the `rapidfind` binary: CLI, terminal UI and `service.rs` (the Windows service)
- `crates/gui`: `rapidfind-gui`: `app.rs` (main window), `preview.rs` (preview pane), `options.rs`, `shell.rs` (icons, context menu, drag-out, dialogs), `highlight.rs`, `results.rs`, `settings.rs`, `tree.rs` (tree mode), `growth.rs` (timeline and details pane); `tests/ui.rs` (end-to-end), `examples/mock_service.rs`
- `crates/mcp`: `rapidfind-mcp`, the MCP server (`tools.rs`) and client registration (`register.rs`)
- `crates/setup`: `rapidfind-setup`, the installer (`install.rs`, `sys.rs`, `ui.rs`)
- `crates/gui/tests/common/demo.rs` and `examples/demo_service.rs`: the made-up demo PC for screenshots
- `packaging/winget`: winget manifest templates; `.github/workflows`: CI and releases
- `tools/`: `build-release.ps1` (installer, .mcpb, winget manifests), `build-mcpb.ps1`, `demo-shots.ps1` and `mcp-images.py` (README images), `bench/` (benchmarks against Everything), `third-party.py` (license notices), `shot.ps1` (window screenshot), `gui-mock.ps1` (GUI against the mock service)

## Prior art
Studied while writing the NTFS layer:
[ntfs-reader](https://crates.io/crates/ntfs-reader) (MIT/Apache-2.0) and
[Indexer++](https://github.com/dfs-minded/indexer-plus-plus) (MIT), plus
voidtools' public notes on Everything (reads the MFT for the initial index and
uses the USN journal for updates).

## Not yet
- ETP/HTTP servers and EFU file lists.
- Content search has no index of its own (Everything doesn't either): every search reads the files again.
- ReFS volumes have no sizes yet, and ReFS hasn't been tested on a real volume (only with synthetic journal records).

