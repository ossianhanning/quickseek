# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

**Status:** applied for; until SignPath Foundation approves the project, releases
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

Quickseek has no telemetry and sends nothing about its users or their files anywhere. It reaches
the network shares, NAS agents and PCs the user adds or pairs with, and once a day (unless turned
off with `quickseek update mode off`) GitHub, to fetch the newest release's signed `update.json`.

## Update signatures

Separately from Authenticode, every release carries `update.json` (each file's SHA-256 and size)
and `update.json.sig`, an Ed25519 signature made by the release workflow with the
`UPDATE_SIGNING_KEY` secret. Installed copies (the Windows service and the NAS agent) update only to
a newer version whose manifest verifies against the public key built into them
([`crates/remote/src/update_key.in`](crates/remote/src/update_key.in)) and whose files match it.
The private key exists only as that secret and an offline backup with the maintainer. See the
[privacy section](README.md#privacy) of the README.
