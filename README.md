# rapidfind

Instant filename search for Windows NTFS volumes, in the style of voidtools'
Everything, written in Rust.

```
rapidfind              # interactive search-as-you-type (default)
rapidfind scan         # build/refresh the index for all NTFS volumes
rapidfind search -l -s size -d "ext:mp4 size:>1gb"
rapidfind bench "*.dll"
rapidfind selftest -v C   # verify against the real filesystem + live updates
rapidfind watch -v C      # dump USN journal records as they happen
rapidfind status
```

Must run elevated: it reads the volume device directly.

## How it is fast

| Phase | Technique |
|---|---|
| Initial index | Read `$MFT` straight off the volume device (`\\.\C:`) instead of walking directories. Run list decoded from MFT record 0 (following `$ATTRIBUTE_LIST` when the MFT itself is fragmented). |
| | `$MFT::$BITMAP` is used to skip unused records and never read chunks with no live records at all. |
| | 4 MiB unbuffered (`FILE_FLAG_NO_BUFFERING`) positional reads into page-aligned buffers, issued by every rayon worker at once (many reads in flight), each parsing its chunk in place (fixups, attribute walk, UTF-16 → UTF-8). |
| | If a volume filter driver rejects raw volume reads (e.g. `gameflt` on Xbox/Store game drives), the same bytes are read from `\\.\PhysicalDriveN` at the volume's disk offset. |
| | Within a chunk, only spans holding in-use records are read. |
| | Raw records give names, parent, size and timestamps in one pass (`FSCTL_ENUM_USN_DATA` is kept as a fallback, but it has no sizes or dates). |
| Staying current | The USN change journal: `FSCTL_READ_USN_JOURNAL` with `BytesToWaitFor = 1` blocks in the kernel until something changes. Records are applied through an MFT-record → entry map, so no per-change path lookups; only files whose data changed get a batched `OpenFileById` size refresh. Directory renames keep their entry, so children follow for free. |
| Restart | The index is saved as raw arrays together with the journal id and next USN (`%LOCALAPPDATA%\rapidfind\C.db`). On startup it's loaded with a few large reads, and only the journal records since the last run are replayed. A rescan happens only if the journal wrapped or was recreated. |
| Search | All folded (lowercased) names live in one contiguous `\0`-separated byte arena. A substring query is a parallel SIMD `memmem` over that arena. Matches map back to entries by galloping search, then are checked against the full query. |
| | Name sorting uses a precomputed per-volume rank (integer compares); top-k is taken per volume before merging. |
| | Paths are built only for results that get displayed, and only the top-N results are sorted (`select_nth_unstable`). |
| | Folder sizes are rolled up at scan time and maintained incrementally. |

## Measured (this machine, 6.7M files on 3 NTFS volumes)

| | |
|---|---|
| Full MFT scan | S: (NVMe, 1.4M records) 1.4–2.6 s · F: (NVMe, via physical disk) 2.7 s · C: (SATA QLC, 4.4M records, 3.7 GiB MFT) ~25 s, bound by the drive |
| Restart (load + journal replay) | C: 0.4–0.7 s, others ~0.2–0.3 s |
| Live update visible | 2–10 ms after the change |
| Query, all volumes | `txt` 14 ms · `*.dll` 39 ms · `windows\system32\ ext:exe` 6 ms · `size:>1gb` 10 ms · `a` (4.2M hits) 182 ms |
| Memory | C: 3.26M entries in 379 MB |
| Correctness | `selftest`: 3,002 random entries checked against the filesystem, 0 missing / 0 size mismatches |

## Query syntax

`foo bar` (AND), `foo|bar` (OR), `!foo` (NOT), `"with space"`, wildcards
`*.rs` / `a?c` (whole name), `users\me` (a `\` matches the full path),
`case:` `path:` `ww:` `regex:` `ext:jpg;png` `size:>10mb` `size:1mb..2mb`
`file:` `folder:`.

## Layout

- `crates/core`: the library
  - `ntfs/`: `mft.rs` (raw scan), `record.rs` (FILE record parsing), `usn.rs` (records + enum fallback), `journal.rs`, `win.rs`
  - `index.rs`, `query.rs`, `search.rs`, `live.rs` (journal → index), `persist.rs`, `engine.rs`
- `crates/cli`: the `rapidfind` binary (CLI + terminal UI)

## Prior art

Studied while writing the NTFS layer:
[ntfs-reader](https://crates.io/crates/ntfs-reader) (MIT/Apache-2.0) and
[Indexer++](https://github.com/dfs-minded/indexer-plus-plus) (MIT), plus
voidtools' public notes on Everything (reads the MFT for the initial index and
uses the USN journal for updates).

## Not yet

- Non-NTFS volumes (FAT/exFAT via directory walk + `ReadDirectoryChangesW`), and ReFS (128-bit ids).
- A background service, so the UI doesn't need elevation, with named-pipe IPC.
- Date filters (`dm:`), content search, and a GUI.
- Removing a single hard link without deleting the file is not tracked precisely yet.
