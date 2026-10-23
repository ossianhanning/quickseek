# A/B comparison of two builds on the same saved index, without admin rights:
# each build's `serve_saved` example serves the index on its own pipe, then
# every query is timed against A and B in turn (ipc_bench, median of -Runs
# after a warm-up), so both see the same machine state. Also prints each
# build's load time and memory once ready and after the queries.
#
#   tools\bench\ab.ps1 -A S:\tmp\target-base\release\examples\serve_saved.exe `
#                      -B target\release\examples\serve_saved.exe -Index $env:LOCALAPPDATA\quickseek\C.db
param(
  [Parameter(Mandatory)] [string]$A,
  [Parameter(Mandatory)] [string]$B,
  [Parameter(Mandatory)] [string[]]$Index,
  [int]$Runs = 25,
  [string[]]$Queries = @("report", "*.pdf", "ext:dll", "size:>1gb", "dm:2026-10-19..2026-10-20", "c:\windows\system32\*.dll", "*\system32\*.dll", "a", "c:\", "c:\ *.jpg")
)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..\..").Path
$bench = Join-Path $root "target\release\examples\ipc_bench.exe"
$logs = @{}
$procs = @{}
foreach ($side in "A", "B") {
  $exe = if ($side -eq "A") { $A } else { $B }
  $log = Join-Path $env:TEMP "ab-$side.log"
  $procs[$side] = Start-Process $exe -ArgumentList (@("--pipe", "\\.\pipe\quickseek-ab-$side") + $Index) -RedirectStandardOutput $log -PassThru -WindowStyle Hidden
  $logs[$side] = $log
}
function Mem($p) { $p.Refresh(); "{0:N0} MB working set, {1:N0} MB private" -f ($p.WorkingSet64 / 1MB), ($p.PrivateMemorySize64 / 1MB) }
try {
  foreach ($side in "A", "B") {
    for ($i = 0; $i -lt 600 -and -not (Select-String -Path $logs[$side] -Pattern "serving on" -Quiet -ErrorAction SilentlyContinue); $i++) { Start-Sleep -Milliseconds 100 }
    Start-Sleep 1
    "$side ($(if ($side -eq 'A') { $A } else { $B })):"
    Get-Content $logs[$side] | Where-Object { $_ -match "entries|ready" } | ForEach-Object { "  $_" }
    "  memory when ready: $(Mem $procs[$side])"
  }
  ""
  "{0,-28} {1,10} {2,10} {3,10} {4,10}" -f "query", "hits", "A ms", "B ms", "B/A"
  foreach ($q in $Queries) {
    $r = @{}
    foreach ($side in "A", "B") {
      $env:QUICKSEEK_PIPE = "\\.\pipe\quickseek-ab-$side"
      $r[$side] = & $bench --runs $Runs $q | ConvertFrom-Json
    }
    "{0,-28} {1,10} {2,10:N1} {3,10:N1} {4,10:N2}" -f $q, $r.B.total, $r.A.median_ms, $r.B.median_ms, ($r.B.median_ms / [math]::Max($r.A.median_ms, 0.001))
    if ($r.A.total -ne $r.B.total) { "  ! hit counts differ: A $($r.A.total), B $($r.B.total)" }
  }
  ""
  foreach ($side in "A", "B") { "$side memory after the queries: $(Mem $procs[$side])" }
} finally {
  $procs.Values | ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
}
