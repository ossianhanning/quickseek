# Releasing

## A release

1. Set the new version in `Cargo.toml` under `[workspace.package]` (every crate inherits it), run the
   tests, and commit (`Cargo.lock` changes with it).
2. Tag it and push the tag: `git tag v0.2.0 && git push origin v0.2.0`.
3. The [release workflow](../.github/workflows/release.yml) checks the tag matches the version,
   tests, builds the programs, signs them (once SignPath is set up), builds and signs the installer,
   packs the Claude Desktop extension and drafts a GitHub release with both. The winget manifests,
   carrying the installer's SHA-256, are kept as a workflow artifact.
4. Check the draft (install it on a clean machine or VM if you can) and publish it.
5. Publishing starts the [winget workflow](../.github/workflows/winget.yml), which opens the pull
   request to [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) once it's enabled
   (below). The first submission of a new package gets a manual review there, usually within a
   few days.

`tools\build-release.ps1 -Repo https://github.com/ossianhanning/rapidfind` does the same packaging
locally, unsigned, into `target\dist`, and validates the manifests with `winget validate`.

## Before the first public release

Both of these need the repository to be public.

### winget

- winget downloads the installer from its public release URL, so it can't be submitted while the
  repository is private.
- Create a classic GitHub token with the `public_repo` scope (wingetcreate forks winget-pkgs to
  your account and opens the pull request from there) and save it as the repository secret
  `WINGET_TOKEN`.
- Set the repository variable `WINGET_SUBMIT` to `true`. Optionally, under Settings → Environments
  → `winget`, add yourself as a required reviewer so each submission waits for a click.
- Or submit by hand: download the manifests artifact from the release run, then
  `wingetcreate submit --token <token> <folder>` (or a pull request with the folder copied to
  `manifests/o/OssianHanning/rapidfind/<version>` in winget-pkgs).
- Check locally with `winget validate --manifest <folder>` and
  `winget install --manifest <folder>` (the latter needs `winget settings --enable LocalManifestFiles`
  once, elevated).

### Code signing with SignPath Foundation

Unsigned installers trigger SmartScreen warnings, and some winget reviewers and users expect signed
binaries. [SignPath Foundation](https://signpath.org) signs open-source projects for free. What it
asks for, and how to apply:

1. **Requirements** (from SignPath Foundation's terms): an OSI-approved license (MIT OR Apache-2.0
   qualifies), a public repository, release builds made by CI from that repository, a code signing
   policy on the project's site ([CODE_SIGNING.md](../CODE_SIGNING.md)), the project is maintained
   and has some users or activity, and no malware, adware or potentially unwanted behaviour. A
   service that runs as SYSTEM and an MCP server that can delete files are legitimate, but describe
   them plainly in the application.
2. **Apply** at [signpath.org/apply](https://signpath.org/apply) with the repository URL, a
   description and the CODE_SIGNING.md link. They review it, which can take some weeks.
3. **Once accepted**, in the SignPath web app:
   - create the project with the slug `rapidfind` and link it to this GitHub repository as a
     trusted build system (GitHub Actions);
   - add two artifact configurations: `programs` (a zip with `rapidfind.exe`, `rapidfind-gui.exe`
     and `rapidfind-mcp.exe`, each signed with Authenticode) and `installer` (a zip with
     `rapidfind-setup.exe`, signed);
   - use the signing policy `release-signing`, with yourself as approver;
   - create an API token for a CI user that may submit to that policy.
4. **In this repository**: add the secret `SIGNPATH_API_TOKEN` and the variable
   `SIGNPATH_ORGANIZATION_ID` (Settings → Secrets and variables → Actions). The release workflow
   signs from then on; without them it builds unsigned, as now.
5. Add the SignPath line to the README if they ask for it ("Free code signing provided by
   SignPath.io, certificate by SignPath Foundation"); CODE_SIGNING.md already has it.

The slugs above (`rapidfind`, `release-signing`, `programs`, `installer`) are what the workflow
uses; if SignPath gives you others, change them in `release.yml`.
