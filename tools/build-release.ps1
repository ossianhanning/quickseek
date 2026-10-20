# Builds a release: binaries, rapidfind-setup.exe (with the binaries
# embedded), the Claude Desktop extension and winget manifests (validated).
#   tools\build-release.ps1 -Repo https://github.com/<owner>/rapidfind
# Output in target\dist:
#   rapidfind-setup-<ver>-x64.exe        the installer (upload to the GitHub release v<ver>)
#   rapidfind-<ver>.mcpb                 Claude Desktop extension
#   winget\manifests\o\OssianHanning\rapidfind\<ver>\*.yaml   for a PR to microsoft/winget-pkgs
param([string]$Repo = "https://github.com/OWNER/rapidfind", [switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").Path
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root "target\dist"
New-Item -ItemType Directory -Force $dist | Out-Null

# cargo reports progress on stderr, which Windows PowerShell would treat as an error.
function Cargo-Build([string[]]$cargoArgs) {
  $ErrorActionPreference = "Continue"
  & cargo @cargoArgs 2>&1 | ForEach-Object { "$_" } | Where-Object { $_ -notmatch "^\s+(Compiling|Building)" }
  if ($LASTEXITCODE) { throw "cargo $($cargoArgs -join " ") failed" }
}

if (-not $SkipBuild) {
  Push-Location $root
  try {
    Cargo-Build @("build", "--release")
    $env:RAPIDFIND_SETUP_PAYLOAD = Join-Path $root "target\release"
    Cargo-Build @("build", "--release", "-p", "rapidfind-setup")
  } finally {
    Remove-Item Env:RAPIDFIND_SETUP_PAYLOAD -ErrorAction SilentlyContinue
    Pop-Location
  }
}

$setupName = "rapidfind-setup-$version-x64.exe"
$setup = Join-Path $dist $setupName
Copy-Item (Join-Path $root "target\release\rapidfind-setup.exe") $setup -Force
& "$PSScriptRoot\build-mcpb.ps1" -Exe (Join-Path $root "target\release\rapidfind-mcp.exe") -OutDir $dist

$sha = (Get-FileHash $setup -Algorithm SHA256).Hash
$url = "$($Repo.TrimEnd('/'))/releases/download/v$version/$setupName"
$out = Join-Path $dist "winget\manifests\o\OssianHanning\rapidfind\$version"
Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $out | Out-Null
foreach ($t in Get-ChildItem "$root\packaging\winget\*.yaml") {
  $text = [IO.File]::ReadAllText($t.FullName).Replace("{VERSION}", $version).Replace("{URL}", $url).Replace("{SHA256}", $sha).Replace("{REPO}", $Repo)
  [IO.File]::WriteAllText((Join-Path $out $t.Name), $text)
}
winget validate --manifest $out
if ($LASTEXITCODE) { throw "winget validate failed" }
Write-Output ""
Write-Output "installer: $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB, sha256 $sha)"
Write-Output "manifests: $out"
if ($Repo -like "*OWNER*") { Write-Output "note: pass -Repo with the real GitHub repository before submitting the manifests" }
