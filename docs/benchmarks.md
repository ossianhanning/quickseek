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
| rapidfind | 0.1.0+c514c0b as the service, 6,913,961 entries on C:, F:, S: |
| Everything | 1.4.1.1024 with its service, 6,686,064 entries, indexing names, sizes and modified dates with fast sorting by size, date and path (comparable to rapidfind's defaults) |

## Method

- **Query latency** is what a client sees: a round trip to each tool's own process over its own IPC.
  For rapidfind that's its named pipe ([`ipc_bench`](../crates/core/examples/ipc_bench.rs)), and for
  Everything the `WM_COPYDATA` query from its SDK's `everything_ipc.h`. Both sort by name and return
  the first 100 results, as a list view would ask. Each query gets one warm-up run, then 41 timed
  runs: the median, and the 10th-90th percentile as the spread. Nothing else ran at the time, and
  `bench.ps1` waits until no rapidfind volume is rescanning.
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

`dm:today` is the exception: rapidfind found 850-1,000 more files modified today. All of them are
in one build-output folder where a compiler keeps creating, renaming and deleting folders of object
files. Everything had those folders, but only 3 of the 98-260 files in each; rapidfind's counts
matched what was on disk (97 of 98, 157 of 157, 259 of 259 in a sample). So Everything had missed
them; it isn't a rapidfind error.

## Results

Round trip in milliseconds, median and 10th-90th percentile of 41 runs; lower is quicker. Hits are
rapidfind (outside Everything's excludes) / Everything. The middle column is an earlier run of the
same build, a few hours before (Everything was slower then on every row, the machine busier); the
035aabe column is the build before the stored orders, median of 15 runs.

| Query | rapidfind c514c0b | Everything | earlier run: rapidfind / Everything | rapidfind 035aabe | Hits |
|---|---:|---:|---:|---:|---|
| `report` (substring) | **4.1** (3.8-5.7) | 18.3 (16.9-20.4) | 4.2 / 36.6 | 7.9 | 7,576 / 7,576 |
| `*.pdf` | **0.8** (0.7-1.1) | 7.2 (6.6-8.2) | 1.2 / 10.7 | 8.5 | 2,473 / 2,473 |
| `ext:dll` | **2.4** (2.2-2.8) | 6.7 (6.1-7.8) | 3.9 / 10.3 | 8.9 | 219,386 / 219,384 |
| `size:>1gb` | **0.2** (0.1-0.3) | 6.1 (5.6-7.2) | 0.2 / 7.8 | 6.6 | 358 / 358 |
| `dm:today` | **1.6** (1.4-1.9) | 7.4 (6.7-8.7) | 1.6 / 10.1 | 7.7 | 29,824 / 29,072 (see above) |
| `c:\windows\system32\*.dll` | **3.7** (3.4-4.7) | 35.8 (33.9-38.8) | 5.3 / 65.6 | 12.6 | 5,012 / 5,012 |
| `*\system32\*.dll` | **5.9** (5.5-7.0) | 183.6 (178.8-190.1) | 8.3 / 290.7 | 23.4 | 6,078 / 6,078 |
| `f:\ *.jpg` | **1.1** (1.0-1.3) | 10.2 (9.7-10.9) | 1.6 / 19.8 | 7.1 | 5,886 / 5,886 |
| `a` (4.2 million hits) | **12.1** (11.3-13.3) | 16.5 (15.2-17.2) | 15.8 / 25.6 | 19.1 | 4,241,917 / 4,242,666 |
| `f:\` (a whole drive, 1.9 million hits) | **5.0** (4.7-6.0) | 13.1 (12.6-14.2) | 6.6 / 21.2 | 9.7 | 1,914,007 / 1,912,735 |

In both runs, on every row rapidfind's 90th percentile is below Everything's 10th.

| | rapidfind c514c0b | Everything | rapidfind 035aabe |
|---|---:|---:|---:|
| Memory, working set / private | 552 / 552 MB | 590 / 579 MB | 488 / 488 MB |
| Index on disk | 75 MB | 260 MB | 74 MB |

The stored orders (below) cost most of the memory difference between c514c0b and 035aabe.

**Cold build** (building the index with nothing saved) is being re-measured. rapidfind builds C:,
F: and S: in parallel in 30-41 s (6.91 M entries). Everything's numbers aren't trustworthy yet: its
"database loaded" signal comes before every volume is indexed, live changes keep its count moving
so "finished" has to allow for that, and its separate benchmark instance came up missing a volume
(S: in one run, F: in the next). `bench.ps1` now reads Everything's own debug log for each
volume's indexing time and any volume it couldn't open, and checks each volume's count against
the normal database. Everything indexes its volumes one after another; rapidfind in parallel.

## Reading the numbers

- Both answer typical searches well under 50 ms, which feels instant while typing.
- rapidfind is quicker on every row: 1.4x on `a`, 2.6-2.8x on `ext:dll` and a whole drive, 4.5x
  on `report` and `dm:today`, 9x on `*.pdf` and `f:\ *.jpg`, 10x on `c:\windows\system32\*.dll`,
  31x on `*\system32\*.dll`, and `size:>1gb` in 0.2 ms against 6.1.
- The biggest changes since the first run are large name-sorted results (`a`, a whole drive), now
  read off a stored name order instead of sorted (10-14x faster), and the stored size, date and
  extension orders (below).
- rapidfind's saved index is under a third of Everything's, and it uses about 30 MB less memory
  for slightly more files.

## Stored size, date and extension orders (e9afa56)

Measured before installing it (the results above are with it).
[`tools/bench/ab.ps1`](../tools/bench/ab.ps1) served the same saved C:
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
- Cost: the orders take about 17 MB more per 3.3 M entries; on the service, 541-552 MB against
  488.

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
