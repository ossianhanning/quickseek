# rapidfind and Everything: benchmarks

[Everything](https://www.voidtools.com/) is the tool rapidfind is most often compared with, and the
one it learned from. These numbers come from one PC and one afternoon. They show where each tool is
quicker, not a winner. Run [`tools/bench/bench.ps1`](../tools/bench/bench.ps1) to measure your own
machine.

## Setup

| | |
|---|---|
| Date | 2026-10-22 |
| CPU | AMD Ryzen 9 7950X3D (16 cores) |
| RAM | 64 GB |
| Disks | NVMe 1 TB (C:), NVMe 2 TB, SATA SSD 1 TB |
| OS | Windows 11 Pro 10.0.26200 |
| rapidfind | 0.1.0+adcf11a, service, 6,968,803 entries on C:, F:, S: |
| Everything | 1.4.1.1024 with its service, 6,437,381 entries, indexing names, sizes and modified dates with fast sorting by size, date and path (comparable to rapidfind's defaults) |

## Method

- **Query latency** is what a client sees: a round trip to each tool's own process over its own IPC.
  For rapidfind that's its named pipe ([`ipc_bench`](../crates/core/examples/ipc_bench.rs)), and for
  Everything the `WM_COPYDATA` query from its SDK's `everything_ipc.h`. Both sort by name and return
  the first 100 results, as a list view would ask. Each query gets one warm-up run, then the median
  of 15 runs.
- **Memory** is the working set and private bytes of the process holding the index, after the
  queries.
- **Index on disk** is rapidfind's `%ProgramData%\rapidfind\index\*.db` and Everything's
  `Everything.db`.
- **Cold build** (scanning a volume with no saved index) needs administrator rights for both tools.
  This run wasn't elevated, so it's **not measured** here. `bench.ps1 -ColdBuild C` measures it when
  run elevated. Everything's `-reindex` rebuilds all of its volumes, not one, so that comparison is
  rough.

**Caveat:** the two indexes don't hold exactly the same files. This PC's Everything settings
exclude one large cloud-synced folder on C:, which rapidfind indexes, and differ in a few other
places. On F: they match (1,918,931 vs 1,912,735 entries, the same 5,886 JPEGs and 486 PDFs). On C:
the hit counts differ, by up to 6× for PDFs, which mostly live in that folder. The `f:\` rows and the
system32 rows compare exactly the same files. For full parity, give both tools the same volumes and
excludes.

## Results

Median round trip in milliseconds over 15 runs; lower is quicker.

| Query | rapidfind | Everything | Hits (rapidfind / Everything) |
|---|---:|---:|---|
| `report` (substring) | **8.8** | 38.1 | 7,801 / 7,270 |
| `*.pdf` | **13.8** | 22.3 | 15,899 / 2,473 ¹ |
| `ext:dll` | 45.6 | **19.2** | 219,813 / 217,187 |
| `size:>1gb` | **6.4** | 9.6 | 453 / 358 ¹ |
| `dm:today` | 21.9 | **11.2** | 34,526 / 17,550 ¹ |
| `c:\windows\system32\*.dll` | **50.1** | 83.1 | 5,012 / 5,011 |
| `*\system32\*.dll` | **35.6** | 291.4 | 6,078 / 6,077 |
| `f:\ *.jpg` | 44.7 | 44.8 | 5,886 / 5,886 |
| `a` (4.4 million hits) | 201.6 | **31.3** | 4,409,487 / 4,101,211 |
| `f:\` (a whole drive, 1.9 million hits) | 155.2 | **44.7** | 1,918,931 / 1,912,735 |

¹ Different files on C: (see the caveat): Everything excludes a folder holding many of these.

| | rapidfind | Everything |
|---|---:|---:|
| Memory, working set / private | 566 / 583 MB | 566 / 582 MB |
| Index on disk | 90 MB | 260 MB |
| Cold build of C: (3.3 M entries) | not measured (needs admin) | not measured (needs admin) |

## Reading the numbers

- Both answer typical searches well under 50 ms, which feels instant while typing.
- rapidfind is quicker on substrings and on path wildcards (`*\system32\*.dll`: 36 ms vs 291 ms).
- Everything is quicker when millions of results come back sorted by name (`a`, a whole drive) and
  on `ext:` and `dm:`, thanks to its presorted indexes. rapidfind sorts a big result set on demand.
- They use about the same memory for the same number of files. rapidfind's saved index is about a
  third of the size (compressed columns).

## What each one does that the other doesn't

| rapidfind | Everything |
|---|---|
| Folder sizes in a live tree, and history: what grew or shrank since yesterday, last week or a save point | Mature: since 2009, with a huge user base and a long track record |
| An MCP server, so AI assistants (Claude, Codex, Cursor, VS Code, ...) can search and free space | An ecosystem: the SDK, `es.exe`, plugins (PowerToys Run, Flow Launcher, ...), the HTTP and ETP servers |
| Cleanup candidates (caches, build output, old downloads) with deletion to the Recycle Bin | File lists, bookmarks, filters, macros, many more search functions |
| Indexes network shares and non-NTFS folders as the connecting user | Runs on very old Windows versions, and portable |
