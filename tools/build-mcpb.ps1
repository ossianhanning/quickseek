# Packs rapidfind-mcp.exe as a Claude Desktop extension (.mcpb, a zip with a
# manifest). Double-clicking the file in Explorer (or Settings → Extensions →
# Install Extension in Claude Desktop) installs it.
#   tools\build-mcpb.ps1                  (uses target\release\rapidfind-mcp.exe)
param([string]$Exe = "$PSScriptRoot\..\target\release\rapidfind-mcp.exe", [string]$OutDir = "$PSScriptRoot\..\target\dist")
$ErrorActionPreference = "Stop"
$Exe = (Resolve-Path $Exe).Path
$version = (Select-String -Path "$PSScriptRoot\..\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value

# Ask the server for its tools, so the manifest lists exactly what it serves.
$req = '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
$reply = ConvertFrom-Json -InputObject ([string]($req | & $Exe))
$tools = @($reply.result.tools | ForEach-Object { [ordered]@{ name = $_.name; description = ($_.description -split '(?<=\.)\s')[0] } })

$manifest = [ordered]@{
  manifest_version = "0.2"
  name = "rapidfind"
  display_name = "rapidfind file search"
  version = $version
  description = "Instant file search and disk-space cleanup over the rapidfind index of every file on your drives."
  long_description = "Finds files by name, path wildcards, size, dates and contents instantly (no disk scan), breaks down which folders use the most space, lists cleanup candidates (temp files, build output, caches, old downloads, duplicates) and deletes to the Recycle Bin on request. Needs the rapidfind service (rapidfind service install)."
  author = [ordered]@{ name = "Ossian Hanning" }
  license = "MIT OR Apache-2.0"
  keywords = @("search", "files", "disk space", "cleanup", "everything", "ntfs")
  server = [ordered]@{
    type = "binary"
    entry_point = "server/rapidfind-mcp.exe"
    mcp_config = [ordered]@{ command = '${__dirname}/server/rapidfind-mcp.exe'; args = @() }
  }
  tools = @($tools)
  compatibility = [ordered]@{ platforms = @("win32") }
}

$stage = Join-Path $env:TEMP "rapidfind-mcpb-$PID"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\server" | Out-Null
Copy-Item $Exe "$stage\server\rapidfind-mcp.exe"
[IO.File]::WriteAllText("$stage\manifest.json", ($manifest | ConvertTo-Json -Depth 6))
New-Item -ItemType Directory -Force $OutDir | Out-Null
$out = Join-Path $OutDir "rapidfind-$version.mcpb"
# Zip entries need forward slashes (Compress-Archive in Windows PowerShell writes backslashes).
Remove-Item $out -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$z = [IO.Compression.ZipFile]::Open($out, "Create")
foreach ($f in Get-ChildItem $stage -Recurse -File) {
  $name = $f.FullName.Substring($stage.Length + 1).Replace("\", "/")
  [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($z, $f.FullName, $name, "Optimal") | Out-Null
}
$z.Dispose()
Remove-Item -Recurse -Force $stage
Write-Output "built $out ($([math]::Round((Get-Item $out).Length / 1MB, 1)) MB, $($tools.Count) tools)"
