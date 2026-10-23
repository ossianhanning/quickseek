# rapidfind and Everything: benchmarks

[Everything](https://www.voidtools.com/) is the tool rapidfind is most often compared with, and the
one it learned from. These numbers come from one PC and one afternoon. They show where each tool is
quicker, not a winner. Run [`tools/bench/bench.ps1`](../tools/bench/bench.ps1) to measure your own
machine.

## Setup

| | |
|---|---|
| Date | 2026-10-23 |
| CPU | AMD Ryzen 9 7950X3D (16 cores) |
| RAM | 64 GB |
| Disks | NVMe 1 TB (C:), NVMe 2 TB, SATA SSD 1 TB |
| OS | Windows 11 Pro 10.0.26200 |
| rapidfind | 0.1.0+9cbc196 as the service, 6,897,769 entries on C:, F:, S:, after its one-time rescan |
| Everything | 1.4.1.1024 with its service, 6,670,958 entries, indexing names, sizes and modified dates with fast sorting by size, date and path (comparable to rapidfind's defaults) |

## Method

- **Query latency** is what a client sees: a round trip to each tool's own process over its own IPC.
  For rapidfind that's its named pipe ([`ipc_bench`](../crates/core/examples/ipc_bench.rs)), and for
  Everything the `WM_COPYDATA` query from its SDK's `everything_ipc.h`. Both sort by name and return
  the first 100 results, as a list view would ask. Each query gets one warm-up run, then the median
  of 15 runs. Nothing else ran at the time, and `bench.ps1` waits until no rapidfind volume is
  rescanning.
- **Memory** is the working set and private bytes of the process holding the index, after the
  queries.
- **Index on disk** is rapidfind's `%ProgramData%\rapidfind\index\*.db` and Everything's
  `Everything.db`.
- **Cold build** (scanning a volume with no saved index) needs administrator rights for both tools.
  It isn't measured yet. `bench.ps1 -ColdBuild C` measures it when run elevated. Everything's
  `-reindex` rebuilds all of its volumes, not one, so that comparison is rough.

**Hit counts.** This PC's Everything settings exclude one large cloud-synced folder on C: that
rapidfind indexes. `bench.ps1` reads that list from Everything's settings and also counts
rapidfind's hits outside it ("outside" below). There the two agree to within the files that
changed between the counts, which shows the queries mean the same thing: `report`, `*.pdf`,
`size:>1gb` and the F: rows exactly, `ext:dll` 219,356 against 219,354, `dm:today` 12,914 against
12,948.

## Results

Median round trip in milliseconds over 15 runs; lower is quicker. Hits are rapidfind (outside
Everything's excludes) / Everything.

| Query | rapidfind 9cbc196 | Everything | rapidfind adcf11a (first run) | Hits |
|---|---:|---:|---:|---|
| `report` (substring) | **8.6** | 40.6 | 8.8 | 7,574 / 7,574 |
| `*.pdf` | **9.8** | 13.3 | 13.8 | 2,473 / 2,473 |
| `ext:dll` | **9.5** | 15.7 | 45.6 | 219,356 / 219,354 |
| `size:>1gb` | **5.4** | 12.3 | 6.4 | 358 / 358 |
| `dm:today` | **7.6** | 13.2 | 21.9 | 12,914 / 12,948 |
| `c:\windows\system32\*.dll` | **15.7** | 71.9 | 50.1 | 5,012 / 5,012 |
| `*\system32\*.dll` | **28.6** | 294.0 | 35.6 | 6,078 / 6,078 |
| `f:\ *.jpg` | **8.2** | 21.3 | 44.7 | 5,886 / 5,886 |
| `a` (4.2 million hits) | **19.9** | 28.8 | 201.6 | 4,231,267 / 4,230,658 |
| `f:\` (a whole drive, 1.9 million hits) | **10.8** | 22.4 | 155.2 | 1,914,007 / 1,912,735 |

Earlier clean runs of 1581c1c gave the same picture (`ext:dll` 6.9 against 12.0 ms, `a` 14.8-21.7
against 36.1).

| | rapidfind 9cbc196 | Everything | rapidfind 1581c1c |
|---|---:|---:|---:|
| Memory, working set / private | 464 / 618 MB ¹ | 587 / 575 MB | 516 / 517 MB |
| Index on disk | 74 MB | 260 MB | 80 MB |
| Cold build of C: (3.3 M entries) | not measured (needs admin) | not measured (needs admin) | |

¹ After 9cbc196's one-time rescan, each rescanned volume's old index stayed in memory until that
drive next changed: its journal watcher held it. Fixed since (the old index is freed when the
new one replaces it). Without a rescan, 1581c1c used 517 MB.

## Reading the numbers

- Both answer typical searches well under 50 ms, which feels instant while typing.
- rapidfind is quicker on every row, about 1.5-2x on most, 4.5x on `c:\windows\system32\*.dll` and
  10x on `*\system32\*.dll`.
- The biggest changes since the first run are large name-sorted results (`a`, a whole drive), now
  read off a stored name order instead of sorted (10-14x faster), and `ext:` (5x).
- rapidfind's saved index is under a third of Everything's. Its memory is about 10% below
  Everything's for slightly more files, once the rescan fix is in.

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
  (format v5 or v6) is rescanned once in the background (format v7); after
  it, `dm:today` hit counts match Everything's (12,914 against 12,948).
- After 9cbc196: a rescan frees the old index at once (before, it stayed
  until the drive changed again), and status shows "rescanning".

**Not measured yet:**
- **Memory after a rescan with that fix.**
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
