# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

**Status:** applied for; until SignPath Foundation approves the project, releases (19.9.0 included)
are built by the same workflow but not signed.

## What is signed

Every release's Windows binaries: `quickseek.exe` (CLI and service), `quickseek-gui.exe`,
`quickseek-mcp.exe` and the installer `quickseek-setup-<version>-x64.exe`, which embeds the other
three after they're signed. Nothing else is signed with this certificate.

## How releases are built

- Release builds come only from the [release workflow](.github/workflows/release.yml) on GitHub
  Actions, started by pushing a version tag to this repository. Nobody signs binaries built on their
  own machine.
- The workflow builds from the tagged commit, runs the tests, and sends the binaries to SignPath. A
  signing request for the release policy waits for a maintainer's approval in SignPath before
  anything is signed.
- The signed installer is attached to a draft GitHub release, checked, and then published. Its
  SHA-256 goes into the winget manifest.

## Team

| Role | Who |
|---|---|
| Committer and reviewer | [Ossian Hanning](https://github.com/ossianhanning) |
| Approver (signing requests) | [Ossian Hanning](https://github.com/ossianhanning) |

All changes reach `main` through commits by the committers above; contributions from others are
reviewed before they're merged.

## Privacy

Quickseek makes no network connections of its own: no telemetry, no update checks, nothing sent
anywhere. It only reaches network shares that the user adds to the index. See the
[privacy section](README.md#privacy) of the README.
