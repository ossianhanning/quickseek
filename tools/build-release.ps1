# Builds a release: binaries, quickseek-setup.exe (with the binaries
# embedded), the Claude Desktop extension and winget manifests (validated).
#   tools\build-release.ps1 -Repo https://github.com/<owner>/quickseek
# Output in target\dist:
#   quickseek-setup-<ver>-x64.exe        the installer (upload to the GitHub release v<ver>)
#   quickseek-<ver>.mcpb                 Claude Desktop extension
#   winget\manifests\o\OssianHanning\Quickseek\<ver>\*.yaml   for a PR to microsoft/winget-pkgs
param([string]$Repo = "https://github.com/OWNER/quickseek", [switch]$SkipBuild)
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
    $env:QUICKSEEK_SETUP_PAYLOAD = Join-Path $root "target\release"
    Cargo-Build @("build", "--release", "-p", "quickseek-setup")
  } finally {
    Remove-Item Env:QUICKSEEK_SETUP_PAYLOAD -ErrorAction SilentlyContinue
    Pop-Location
  }
}

$setupName = "quickseek-setup-$version-x64.exe"
$setup = Join-Path $dist $setupName
Copy-Item (Join-Path $root "target\release\quickseek-setup.exe") $setup -Force
& "$PSScriptRoot\build-mcpb.ps1" -Exe (Join-Path $root "target\release\quickseek-mcp.exe") -OutDir $dist

$sha = (Get-FileHash $setup -Algorithm SHA256).Hash
$url = "$($Repo.TrimEnd('/'))/releases/download/v$version/$setupName"
$out = Join-Path $dist "winget\manifests\o\OssianHanning\Quickseek\$version"
Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $out | Out-Null
foreach ($t in Get-ChildItem "$root\packaging\winget\*.yaml") {
  $text = [IO.File]::ReadAllText($t.FullName).Replace("{VERSION}", $version).Replace("{URL}", $url).Replace("{SHA256}", $sha).Replace("{REPO}", $Repo)
  [IO.File]::WriteAllText((Join-Path $out $t.Name), $text)
}
# winget compares PackageVersion with what the installer writes to Apps & Features
# (DisplayVersion); a mismatch fails validation. All come from Cargo.toml: check.
$exeVersion = (Get-Item $setup).VersionInfo.ProductVersion
if ($exeVersion -ne $version) { throw "installer file version '$exeVersion' isn't $version" }
foreach ($m in Get-ChildItem $out -Filter *.yaml) {
  foreach ($line in Select-String -Path $m.FullName -Pattern '^\s*(PackageVersion|DisplayVersion):\s*(.+)$') {
    if ($line.Matches[0].Groups[2].Value.Trim() -ne $version) { throw "$($m.Name): $($line.Line.Trim()) doesn't match $version" }
  }
}
if (Get-Command winget -ErrorAction SilentlyContinue) {
  # An older winget (GitHub's runners) warns about schemas newer than it
  # knows and exits non-zero; only a failed validation stops the build.
  $v = @(winget validate --manifest $out 2>&1 | ForEach-Object { "$_" })
  $v | ForEach-Object { Write-Output $_ }
  if (-not ($v -match "Manifest validation succeeded")) { throw "winget validate failed" }
} else {
  Write-Output "note: winget isn't installed here, so the manifests weren't validated (winget validate --manifest $out)"
}
Write-Output ""
Write-Output "installer: $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB, sha256 $sha)"
Write-Output "manifests: $out"
if ($Repo -like "*OWNER*") { Write-Output "note: pass -Repo with the real GitHub repository before submitting the manifests" }
