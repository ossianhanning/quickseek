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
| rapidfind | 0.1.0+035aabe as the service, 6,908,157 entries on C:, F:, S: |
| Everything | 1.4.1.1024 with its service, 6,680,173 entries, indexing names, sizes and modified dates with fast sorting by size, date and path (comparable to rapidfind's defaults) |

## Method

- **Query latency** is what a client sees: a round trip to each tool's own process over its own IPC.
  For rapidfind that's its named pipe ([`ipc_bench`](../crates/core/examples/ipc_bench.rs)), and for
  Everything the `WM_COPYDATA` query from its SDK's `everything_ipc.h`. Both sort by name and return
  the first 100 results, as a list view would ask. Each query gets one warm-up run, then the median
  of 15 runs (`bench.ps1` now does 41 and also prints each tool's 10th-90th percentile spread).
  Nothing else ran at the time, and `bench.ps1` waits until no rapidfind volume is rescanning.
- **Memory** is the working set and private bytes of the process holding the index, after the
  queries.
- **Index on disk** is rapidfind's `%ProgramData%\rapidfind\index\*.db` and Everything's
  `Everything.db`.
- **Cold build** (building the index with nothing saved) needs administrator rights for both tools.
  rapidfind scans in its own process, ignoring the saved index; Everything runs as a separate
  instance with its own database and `-reindex`.

**Hit counts.** This PC's Everything settings exclude one large cloud-synced folder on C: that
rapidfind indexes. `bench.ps1` reads that list from Everything's settings and also counts
rapidfind's hits outside it ("outside" below). There the two agree to within the files that
changed between the counts, which shows the queries mean the same thing.

`dm:today` is the exception: rapidfind found about 1,000 more files modified today. All of them are
in one build-output folder where a compiler keeps creating, renaming and deleting folders of object
files. Everything had those folders, but only 3 of the 98-260 files in each; rapidfind's counts
matched what was on disk (97 of 98, 157 of 157, 259 of 259 in a sample). So Everything had missed
them; it isn't a rapidfind error.

## Results

Median round trip in milliseconds over 15 runs; lower is quicker. Hits are rapidfind (outside
Everything's excludes) / Everything.

| Query | rapidfind 035aabe | Everything | rapidfind 9cbc196 | rapidfind adcf11a (first run) | Hits |
|---|---:|---:|---:|---:|---|
| `report` (substring) | **7.9** | 26.4 | 8.6 | 8.8 | 7,576 / 7,576 |
| `*.pdf` | **8.5** | 9.3 | 9.8 | 13.8 | 2,473 / 2,473 |
| `ext:dll` | 8.9 | 8.9 | 9.5 | 45.6 | 219,377 / 219,375 |
| `size:>1gb` | **6.6** | 8.0 | 5.4 | 6.4 | 358 / 358 |
| `dm:today` | **7.7** | 9.1 | 7.6 | 21.9 | 23,352 / 22,342 (see above) |
| `c:\windows\system32\*.dll` | **12.6** | 52.9 | 15.7 | 50.1 | 5,012 / 5,012 |
| `*\system32\*.dll` | **23.4** | 248.6 | 28.6 | 35.6 | 6,078 / 6,078 |
| `f:\ *.jpg` | **7.1** | 17.3 | 8.2 | 44.7 | 5,886 / 5,886 |
| `a` (4.2 million hits) | **19.1** | 22.5 | 19.9 | 201.6 | 4,237,411 / 4,238,024 |
| `f:\` (a whole drive, 1.9 million hits) | **9.7** | 20.5 | 10.8 | 155.2 | 1,914,007 / 1,912,735 |

Everything was quicker in this run than in the 9cbc196 one (`report` 26.4 against 40.6 ms,
`ext:dll` 8.9 against 15.7), so the gap is narrower on the simple rows: `ext:dll` was even, and
`*.pdf` and `size:>1gb` within 1.5 ms. The next build (below) targets those.

| | rapidfind 035aabe | Everything | rapidfind 9cbc196 |
|---|---:|---:|---:|
| Memory, working set / private | 488 / 488 MB | 590 / 578 MB | 464 / 618 MB ¹ |
| Index on disk | 74 MB | 260 MB | 74 MB |
| Cold build | 27.3 s for C: (3.32 M entries) | 161.1 s for every volume it indexes ² | |

¹ After 9cbc196's one-time rescan, each rescanned volume's old index stayed in memory until that
drive next changed: its journal watcher held it. 035aabe frees it when the new index replaces it.

² Not comparable yet: Everything's `-reindex` rebuilds all its volumes (about 6.7 M entries), not
one. `bench.ps1 -ColdBuild all` now builds the same set of volumes with both tools (rapidfind's in
parallel, as its service does), and also rebuilds Everything with a copy of its settings that
includes only C:.

## Reading the numbers

- Both answer typical searches well under 50 ms, which feels instant while typing.
- rapidfind was quicker on every row but `ext:dll` (even), 3x on `report`, 4x on
  `c:\windows\system32\*.dll` and 10x on `*\system32\*.dll`.
- The biggest changes since the first run are large name-sorted results (`a`, a whole drive), now
  read off a stored name order instead of sorted (10-14x faster), and `ext:` (5x).
- rapidfind's saved index is under a third of Everything's, and it uses about 100 MB less memory
  for slightly more files.

## Next build: stored size, date and extension orders (e9afa56)

Not on the service yet. [`tools/bench/ab.ps1`](../tools/bench/ab.ps1) served the same saved C:
index three times over (9.8 M entries) from 035aabe (A) and e9afa56 (B), each over its own pipe,
median of 41 round trips each:

| Query | A (ms) | B (ms) | B/A |
|---|---:|---:|---:|
| `report` | 8.3 | 8.6 | 1.04 |
| `*.pdf` | 11.8 | 3.3 | 0.28 |
| `ext:dll` (481 K hits) | 11.1 | 4.8 | 0.44 |
| `size:>1gb` | 7.8 | 0.2 | 0.03 |
| `c:\windows\system32\*.dll` | 38.0 | 15.7 | 0.41 |
| `*\system32\*.dll` | 35.9 | 15.0 | 0.42 |
| `a` (6.1 M hits) | 21.6 | 20.8 | 0.96 |
| no match (`zzqqxxnotthere`) | 4.5 | 4.3 | 0.97 |

- The sort orders by size and date modified are kept as orders too (position to entry), so a
  size or date range (`size:>1gb`, `dm:today`) is two binary searches instead of a pass over every
  entry. Entries changed since the orders were computed are checked on their own.
- An extension order (entries grouped by extension, in name order) answers `ext:dll`, `*.pdf` and
  path patterns ending in `*.ext` from their range instead of scanning every name.
- Path patterns cache what they've seen per folder in a table that costs only what a query
  touches, not 8 bytes per entry cleared for every query.
- Small candidate lists are checked and sorted on the calling thread: waking the thread pool
  costs more than the work.
- The floor: a search for a name substring reads every name once (about 75 MB per 3.3 M entries).
  That takes about 1 ms per volume at this PC's memory bandwidth, so `report` and a search with no
  match stay at 3-4 ms over three volumes. The new orders don't help there.
- Cost: the orders take about 17 MB more per 3.3 M entries (about 36 MB for this PC's 6.9 M).

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

- After 035aabe's run: stored size, date and extension orders, see
  "Next build" above.

## What each one does that the other doesn't

| rapidfind | Everything |
|---|---|
| Folder sizes in a live tree, and history: what grew or shrank since yesterday, last week or a save point | Mature: since 2009, with a huge user base and a long track record |
| An MCP server, so AI assistants (Claude, Codex, Cursor, VS Code, ...) can search and free space | An ecosystem: the SDK, `es.exe`, plugins (PowerToys Run, Flow Launcher, ...), the HTTP and ETP servers |
| Cleanup candidates (caches, build output, old downloads) with deletion to the Recycle Bin | File lists, bookmarks, filters, macros, many more search functions |
| Indexes network shares and non-NTFS folders as the connecting user | Runs on very old Windows versions, and portable |
