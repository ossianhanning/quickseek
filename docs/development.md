# Development
Building, testing and releasing rapidfind.

## Building

Windows 10 or 11 with Rust (MSVC); `rust-toolchain.toml` pins the version CI uses, and rustup installs it on first build. `cargo build --release` builds the CLI and service
(`rapidfind.exe`), the window (`rapidfind-gui.exe`) and the MCP server (`rapidfind-mcp.exe`) into
`target\release`. The installer embeds those binaries, so it's built by `tools\build-release.ps1`
(see below) rather than by `cargo build`. To run a development build against the real index, install
the service from it once, elevated: `target\release\rapidfind.exe service install`.

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
- content search over a real temp folder: UTF-8 and UTF-16 matches, binaries skipped, `case:`/`!content:`, query errors, stopping, then the preview pane (text with matches, a thumbnail)
- header clicks sort by size, both directions
- deep paging to the last row
- the filter dropdown
- Match Case and Match Diacritics
- live updates
- the context menu's Copy Full Path, checked on the clipboard
- Options → Exclude adding a pattern the service applies
- `Esc` clears the search
- comparing the tree with a month of seeded history: the growth drill-down, the Change column, stopping
- deleting a 3,000-file folder from the tree with `Del`: the window keeps answering (21 ms at worst while 3,000 files went in 3.5 s), the row greys out, and the live tree drops it when the index changes

It opens a window and clicks, so don't type while it runs.

Also:

- `$env:RAPIDFIND_TEST_UNC='\\localhost\S\some\dir'; cargo test -p rapidfind-core --release unc_share -- --ignored` indexes and watches a real SMB share.
- `cargo run -p rapidfind-gui --example mock_service -- --live` serves a synthetic index. Start the GUI with `RAPIDFIND_PIPE=\\.\pipe\rapidfind-mock` to try it without the service.
- `rapidfind selftest` (elevated) checks the real index against the filesystem and exercises live updates.

## The demo PC

`cargo run --release -p rapidfind-gui --example demo_service` serves a made-up Windows PC (a user `alex`,
three drives, about 290,000 files and six weeks of folder-size history) on `\\.\pipe\rapidfind-demo`.
Point the GUI, the CLI or `rapidfind-mcp` at it with `RAPIDFIND_PIPE=\\.\pipe\rapidfind-demo`. The data is
built in [`crates/gui/tests/common/demo.rs`](../crates/gui/tests/common/demo.rs); a test keeps its history
consistent with what the real tree computes.

The README images come from it, never from a real machine:

- `tools\demo-shots.ps1` captures the GUI shots and the frames of the typing animation;
  `python tools\frames-to-anim.py` makes the animation and `python tools\optimize-images.py` shrinks the PNGs.
- `python tools\mcp-images.py` asks `rapidfind-mcp` on the demo PC "where did my space go" and renders the
  chat illustration and the chart with headless Edge.

`name.png` is the high-resolution image (the display's scale for GUI shots, 2x for the MCP images) and
`name@1x.png` the 96-DPI one.

## Benchmarks

`tools\bench\bench.ps1` compares query latency, memory and index size with Everything on your machine;
see [benchmarks.md](benchmarks.md).

## Releases

Tag a version and GitHub Actions builds, signs and drafts the release; see
[releasing.md](releasing.md), which also lists what's needed before the first public release
(winget, code signing). `tools\build-release.ps1` packages a release locally. The winget manifest
templates are in `packaging\winget`.

## Third-party notices

`python tools\third-party.py` regenerates [THIRD-PARTY.md](../THIRD-PARTY.md) from `Cargo.lock` and fails on
a license outside the permissive allow-list; CI checks it's current. The installer ships it with the
license files.
