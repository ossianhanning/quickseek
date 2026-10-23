# rapidfind and Everything: benchmarks

[Everything](https://www.voidtools.com/) is the tool rapidfind is most often compared with, and the
one it learned from. These numbers come from one PC and one afternoon. They show where each tool is
quicker, not a winner. Run [`tools/bench/bench.ps1`](../tools/bench/bench.ps1) to measure your own
machine.

## Setup

| | |
|---|---|
| Date | 2026-10-23 (earlier runs: 2026-10-22 and earlier on 2026-10-23) |
| CPU | AMD Ryzen 9 7950X3D (16 cores) |
| RAM | 64 GB |
| Disks | NVMe 1 TB (C:), NVMe 2 TB, SATA SSD 1 TB |
| OS | Windows 11 Pro 10.0.26200 |
| rapidfind | 0.1.0+1581c1c as the service, 6,988,455 entries on C:, F:, S: |
| Everything | 1.4.1.1024 with its service, 6,670,729 entries, indexing names, sizes and modified dates with fast sorting by size, date and path (comparable to rapidfind's defaults) |

## Method

- **Query latency** is what a client sees: a round trip to each tool's own process over its own IPC.
  For rapidfind that's its named pipe ([`ipc_bench`](../crates/core/examples/ipc_bench.rs)), and for
  Everything the `WM_COPYDATA` query from its SDK's `everything_ipc.h`. Both sort by name and return
  the first 100 results, as a list view would ask. Each query gets one warm-up run, then the median
  of 15 runs. Nothing else ran a benchmark at the same time.
- **Memory** is the working set and private bytes of the process holding the index, after the
  queries.
- **Index on disk** is rapidfind's `%ProgramData%\rapidfind\index\*.db` and Everything's
  `Everything.db`.
- **Cold build** (scanning a volume with no saved index) needs administrator rights for both tools.
  It isn't measured yet. `bench.ps1 -ColdBuild C` measures it when run elevated. Everything's
  `-reindex` rebuilds all of its volumes, not one, so that comparison is rough.

**Hit counts.** The two indexes don't hold exactly the same files: this PC's Everything settings
exclude one large cloud-synced folder on C: that rapidfind indexes. `bench.ps1` reads that list from
Everything's settings and also counts rapidfind's hits outside it ("outside" below). There the counts
agree, which shows the queries mean the same thing:
- `*.pdf` and `size:>1gb` match exactly;
- `ext:dll` gives 219,363 against 219,354;
- the F: rows match entry for entry.

The rest is files changing between the two counts. `dm:today` still doesn't match: the installed
index carries entries from earlier builds for hard links that Rust builds had removed, and new hard
links dated at the journal's time. Both bugs are fixed, and the next build rescans an index saved by
an affected build once, so that row needs measuring again after it.

## Results

Median round trip in milliseconds over 15 runs; lower is quicker. Hits are rapidfind (outside
Everything's excludes) / Everything.

| Query | rapidfind 1581c1c | Everything | rapidfind 5a01068 | rapidfind adcf11a | Hits |
|---|---:|---:|---:|---:|---|
| `report` (substring) | **5.6** | 34.2 | 6.5 | 8.8 | 7,576 / 7,574 |
| `*.pdf` | **6.9** | 13.2 | 7.2 | 13.8 | 2,473 / 2,473 |
| `ext:dll` | **6.9** | 12.0 | 28.2 | 45.6 | 219,363 / 219,354 |
| `size:>1gb` | **5.2** | 9.6 | 6.4 | 6.4 | 358 / 358 |
| `dm:today` ¹ | 5.2 | 10.5 | 6.7 | 21.9 | 23,554 / 11,946 |
| `c:\windows\system32\*.dll` | **14.6** | 62.8 | 17.6 | 50.1 | 5,012 / 5,012 |
| `*\system32\*.dll` | **26.5** | 266.4 | 34.7 | 35.6 | 6,078 / 6,078 |
| `f:\ *.jpg` | **5.0** | 22.1 | 7.5 | 44.7 | 5,886 / 5,886 |
| `a` (4.3 million hits) | **21.7** ² | 36.1 | 18.7 | 201.6 | 4,314,172 / 4,230,515 |
| `f:\` (a whole drive, 1.9 million hits) | **12.5** | 23.4 | 14.1 | 155.2 | 1,918,931 / 1,912,735 |

¹ Not comparable yet: see "Hit counts".
² 14.8-15.7 ms in three more rounds of 25 with Everything idle. One run gave 34.4 ms, measured
  minutes after a service restart while a second benchmark ran; not seen since.

| | rapidfind 1581c1c | Everything | rapidfind adcf11a |
|---|---:|---:|---:|
| Memory, working set / private | 516 / 517 MB | 582 / 568 MB | 566 / 583 MB |
| Index on disk | 80 MB | 260 MB | 90 MB |
| Cold build of C: (3.3 M entries) | not measured (needs admin) | not measured (needs admin) | |

## Reading the numbers

- Both answer typical searches well under 50 ms, which feels instant while typing.
- rapidfind 1581c1c is quicker on every comparable row, by about 2x on most and 10x on path
  wildcards (`*\system32\*.dll`: 27 against 266 ms).
- The biggest changes since the first run are large name-sorted results (`a`, a whole drive), now
  read off a stored name order instead of sorted (10-15x faster), and `ext:` (6.5x).
- rapidfind uses about 10% less memory than Everything for slightly more files, and its saved
  index is under a third of the size.

## Optimization round (2026-10-23)

These are the changes between 0.1.0+adcf11a and 5a01068, measured on this
PC's real data without installing anything, before the results above were
taken on the installed service.

**Method.** [`tools/bench/ab.ps1`](../tools/bench/ab.ps1) serves the same
saved index (C:, 3,273,671 entries, saved by the old build) from two builds
at once, each over its own pipe the way the service would
([`serve_saved`](../crates/core/examples/serve_saved.rs)), and times every
query against both in turn (median of 25 round trips after a warm-up), so
both see the same machine state. A = the old build (`main`), B = the new
one. Hit counts are identical between them.

C: alone, quiet machine:

| Query | A (ms) | B (ms) | B/A |
|---|---:|---:|---:|
| `report` | 1.5 | 1.6 | 1.13 |
| `*.pdf` | 3.3 | 2.4 | 0.73 |
| `ext:dll` | 9.0 | 5.8 | 0.64 |
| `size:>1gb` | 2.1 | 2.3 | 1.09 |
| `dm:2026-10-19..2026-10-20` | 3.9 | 2.0 | 0.52 |
| `c:\windows\system32\*.dll` | 8.2 | 7.7 | 0.95 |
| `*\system32\*.dll` | 7.8 | 7.8 | 1.00 |
| `c:\ *.jpg` | 10.4 | 7.4 | 0.71 |
| `a` (2.0 M hits) | 16.0 | 5.2 | 0.33 |
| `c:\` (3.3 M hits) | 47.2 | 11.2 | 0.24 |

Closer to the service: C: served twice (C: and D:, 6.5 M entries, about
Everything's 6.44 M) with 2,000 entries per volume renamed after ranking, as
a live service always has some changes since its last ranking. The old build
then sorts big name-sorted results by comparing names, which is why `a`
took 202 ms on the installed service:

| Query | A (ms) | B (ms) | B/A | Everything above (ms) |
|---|---:|---:|---:|---:|
| `report` | 4.2 | 4.2 | 0.99 | 38.1 |
| `*.pdf` | 7.8 | 5.9 | 0.75 | 22.3 |
| `ext:dll` (320 K hits) | 16.1 | 12.1 | 0.75 | 19.2 |
| `size:>1gb` | 3.8 | 4.3 | 1.11 | 9.6 |
| date range (65 K hits) | 6.9 | 4.2 | 0.60 | 11.2 (`dm:today`, 17.5 K hits) |
| `c:\windows\system32\*.dll` | 17.7 | 16.1 | 0.91 | 83.1 |
| `*\system32\*.dll` | 16.4 | 16.2 | 0.98 | 291.4 |
| `c:\ *.jpg` (154 K hits) | 19.6 | 15.4 | 0.79 | 44.8 (`f:\ *.jpg`, 5.9 K hits) |
| `a` (4.1 M hits) | 80.2 | 11.6 | 0.15 | 31.3 |
| `c:\` (6.5 M hits) | 160.6 | 22.9 | 0.14 | 44.7 (`f:\`, 1.9 M hits) |
| `d:\` (a drive with no hits) | 42.6 | 0.0 | 0 | |

Ratios in the 0.9-1.1 band are run-to-run noise on this machine (the same
query moves that much between runs). The last column is the earlier
Everything measurement on the real drives, for orientation only: different
files and hit counts, so not a comparison.

| | A (old) | B (new) |
|---|---:|---:|
| Memory when ready, C: | 252 MB | 217 MB |
| Memory after the queries, C: | 293 MB | 228 MB |
| Memory when ready, 6.5 M entries | 503 MB | 441 MB |
| Index on disk, C: | 39.8 MB (v5) | 34.5 MB (v6) |
| Load C: | 0.23-0.39 s | 0.26-0.39 s |
| Ranks ready after load | 0.77-0.99 s | 0.73-0.95 s |
| Build a 3.3 M entry index from a scan (`synth`) | 2.0-2.05 s | 1.94-2.25 s |
| GUI's slowest answer while deleting 3,000 files | 21 ms (earlier note) | 5.9 ms |

**What changed:**
- Name-sorted results come from a stored name order instead of a sort.
- `ext:` scans for the extension at the end of names.
- Whole-volume scans are chunked.
- Path patterns stop reading names below a folder that already matches.
- A drive in a path term skips the other volumes.
- Dates are kept as 4-byte seconds (index format v6).
- Names that differ only in case are kept as a bit mask.

The commit messages on the `perf-vs-everything` branch have the numbers for
each step.

**After 5a01068:**
- `ext:dll`: a lone extension is answered from bitsets (a folder bitset
  and the exclusion bitset) instead of evaluating the query per candidate,
  a cache miss each: 28.2 -> 6.9 ms on the service (1581c1c).
- Journal: a new hard link's size and dates are read from the file, and
  hard links that were removed are dropped (they stayed before).
- After 1581c1c: an index saved by a build that kept removed hard links
  (format v5 or v6) is rescanned once in the background (format v7).

**Not measured yet:**
- **`dm:today` after that rescan.**
- **Cold index build:** needs administrator rights for both tools. The
  scan code is unchanged and building from a scan takes the same time, so
  rapidfind's cold build should be where it was.

## What each one does that the other doesn't

| rapidfind | Everything |
|---|---|
| Folder sizes in a live tree, and history: what grew or shrank since yesterday, last week or a save point | Mature: since 2009, with a huge user base and a long track record |
| An MCP server, so AI assistants (Claude, Codex, Cursor, VS Code, ...) can search and free space | An ecosystem: the SDK, `es.exe`, plugins (PowerToys Run, Flow Launcher, ...), the HTTP and ETP servers |
| Cleanup candidates (caches, build output, old downloads) with deletion to the Recycle Bin | File lists, bookmarks, filters, macros, many more search functions |
| Indexes network shares and non-NTFS folders as the connecting user | Runs on very old Windows versions, and portable |
