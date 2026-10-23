# Changelog

## 19.9.0 (2026-10-24)

The first public release.

### What Quickseek does

- **Instant search on every drive.**
  - Results appear as you type, from an index of every NTFS and ReFS drive.
  - USB drives, folders and network shares can be added too.
  - The index is kept current from each drive's change journal.
  - Search by name, folder, wildcards across folders (`users\*\*.pdf`), size, dates, duplicates,
    regex, or the text inside files.
- **See where your space went.**
  - A live folder-size tree of every drive.
  - Daily snapshots show what grew or shrank since yesterday, last week or a save point.
  - Views: a timeline, a "where it went" breakdown, a treemap and a calendar.
- **Works with AI assistants.**
  - An MCP server for Claude Code, Claude Desktop, Codex, Cursor, VS Code, Windsurf and Antigravity.
  - Assistants can find files, explain what fills a drive (with a ready-made chart) and suggest
    cleanups.
  - Files go to the Recycle Bin only after you approve.
  - The installer connects the assistants it finds.
- **One install for everything.**
  - A background service keeps the index, so searching needs no administrator rights.
  - The window: search as you type, Explorer context menu, preview pane, a light and a dark
    theme, a global hotkey (`Ctrl+Alt+Space`).
  - The `quickseek` command line.
- **Private.**
  - Everything runs on your PC.
  - No telemetry, no update checks, no network use beyond shares you add.

### Against Everything

Measured on one PC with about 6.9 million files on three drives. The details and method are in
[docs/benchmarks.md](docs/benchmarks.md).

| | Quickseek | Everything 1.4 |
|---|---:|---:|
| `report` (substring) | 4.1 ms | 18.3 ms |
| `ext:dll` (219,000 hits) | 2.4 ms | 6.7 ms |
| `size:>1gb` | 0.2 ms | 6.1 ms |
| `*\system32\*.dll` | 5.9 ms | 183.6 ms |
| `a` (4.2 million hits, sorted by name) | 12.1 ms | 16.5 ms |
| Index on disk | 75 MB | 260 MB |
| Building the index of C: from scratch (3.3 M files) | about 27 s | about 153 s |

Times are the median round trip of 41 runs.

### Notes

- **Unsigned.** This release is not code-signed yet (signing with SignPath Foundation is pending),
  so Windows SmartScreen may warn when you run the installer.
- **Formerly rapidfind.** Quickseek was developed under the name rapidfind. Installing it over a
  rapidfind install moves the index, history, settings and AI assistant connections over
  ([how](docs/user-guide.md#moving-over-from-rapidfind)).
