# rapidfind vs Everything on this PC: query latency, memory, index size and
# (elevated only) a cold index build. Prints numbers only, no file names.
#
#   tools\bench\bench.ps1                     queries, memory, index size
#   tools\bench\bench.ps1 -Runs 101           more runs per query
#   tools\bench\bench.ps1 -ColdBuild all      also time cold builds (run elevated)
#   tools\bench\bench.ps1 -ColdBuild C        ... of C: alone
#
# Cold builds: rapidfind scans the volumes (in parallel, as its service
# does), and a separate Everything instance with its own database reindexes.
# "all" means the local volumes Everything's settings include, so both build
# the same drives; Everything also rebuilds with a copy of its settings that
# includes only C:, the like-for-like single-volume number.
#
# Query latency is what a client sees: a round trip to each tool's own
# process over its own IPC (rapidfind's pipe, Everything's WM_COPYDATA
# query), sorted by name, first 100 results. One warm-up run, then the
# median of -Runs, and the 10th-90th percentile spread. rapidfind needs its service running; Everything is
# started (tray, no window) if it isn't, and loads its existing database.
#
# Every line also goes to -Log as it happens (default target\tmp\bench-<time>.txt).
# It waits for rapidfind to finish any background rescan first, every step
# has a time limit, and it only closes the Everything instances it started.
param(
  [int]$Runs = 41,
  [string[]]$Queries = @("report", "*.pdf", "ext:dll", "size:>1gb", "dm:today", "c:\windows\system32\*.dll", "*\system32\*.dll", "f:\ *.jpg", "a", "f:\"),
  [string]$ColdBuild = "",
  [string]$Everything = "C:\Program Files\Everything\Everything.exe",
  [string]$Log = "",
  [int]$WaitMinutes = 30
)
# Never "Stop": Windows PowerShell turns a native program's stderr into errors.
$ErrorActionPreference = "Continue"
$root = (Resolve-Path "$PSScriptRoot\..\..").Path
$rapidfind = Join-Path $root "target\release\rapidfind.exe"
$bench = Join-Path $root "target\release\examples\ipc_bench.exe"
if (-not $Log) {
  New-Item -ItemType Directory -Force (Join-Path $root "target\tmp") | Out-Null
  $Log = Join-Path $root ("target\tmp\bench-{0:yyyyMMdd-HHmmss}.txt" -f (Get-Date))
}
function Say([string]$s) { $s; Add-Content -Path $Log -Value $s -Encoding UTF8 }
Say ("bench.ps1 started {0:yyyy-MM-dd HH:mm:ss}; log: {1}" -f (Get-Date), $Log)

# Runs a program with a time limit; its output goes to files. Returns
# (exit code or $null on timeout, seconds, stdout lines, stderr lines).
function Run-Step([string]$name, [string]$exe, [string[]]$argv, [int]$timeoutSec) {
  $out = Join-Path $env:TEMP "bench-step-out.txt"; $err = Join-Path $env:TEMP "bench-step-err.txt"
  Remove-Item $out, $err -ErrorAction SilentlyContinue
  $t = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $exe -ArgumentList $argv -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -NoNewWindow
  $null = $p.Handle   # kept open, or ExitCode reads empty after exit
  $code = $null
  if ($p.WaitForExit($timeoutSec * 1000)) { $code = $p.ExitCode }
  else {
    try { Stop-Process -Id $p.Id -Force -ErrorAction Stop } catch { }
    Say "  ${name}: no answer after $timeoutSec s, stopped"
  }
  [pscustomobject]@{
    Code = $code; Seconds = [math]::Round($t.Elapsed.TotalSeconds, 1)
    Out = @(Get-Content $out -ErrorAction SilentlyContinue); Err = @(Get-Content $err -ErrorAction SilentlyContinue)
  }
}

# ipc_bench in a background job: arguments pass through intact (quotes in
# queries), with a time limit. Returns its output lines.
function Run-Bench([string[]]$argv, [int]$timeoutSec) {
  $j = Start-Job -ScriptBlock { param($exe, $a) & $exe @a } -ArgumentList $bench, $argv
  if (Wait-Job $j -Timeout $timeoutSec) { $o = @(Receive-Job $j) } else { Say "  ipc_bench: no answer after $timeoutSec s, stopped"; Stop-Job $j; $o = @() }
  Remove-Job $j -Force
  $o | ForEach-Object { "$_" }
}

if (-not (Test-Path $bench)) {
  Say "building ipc_bench"
  $r = Run-Step "cargo" "cargo" @("build", "--release", "-p", "rapidfind-core", "--example", "ipc_bench", "--manifest-path", (Join-Path $root "Cargo.toml")) 900
  if ($r.Code -ne 0) { Say "building ipc_bench failed"; exit 1 }
}

Add-Type -ReferencedAssemblies System.Windows.Forms @"
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Windows.Forms;

// Everything's IPC (its SDK's everything_ipc.h): a query in WM_COPYDATA to
// its tray window, the result list comes back in WM_COPYDATA to ours.
public class EverythingIpc : NativeWindow, IDisposable {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindow(string cls, string title);
  [DllImport("user32.dll")] static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, ref COPYDATASTRUCT l);
  [DllImport("user32.dll")] static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] struct COPYDATASTRUCT { public IntPtr dwData; public int cbData; public IntPtr lpData; }
  const int WM_COPYDATA = 0x4A, WM_USER = 0x400, IS_DB_LOADED = 401, IS_DB_BUSY = 402, GET_MAJOR = 0, GET_MINOR = 1, GET_REVISION = 2, GET_BUILD = 3;
  const int COPYDATAQUERYW = 2, REPLY_ID = 0x52464231;
  public string Class = "EVERYTHING_TASKBAR_NOTIFICATION";
  bool got; public uint TotalItems;

  public EverythingIpc() { CreateHandle(new CreateParams { Parent = (IntPtr)(-3) }); }
  public void Dispose() { DestroyHandle(); }
  IntPtr Target() { return FindWindow(Class, null); }
  public bool Running() { return Target() != IntPtr.Zero; }
  public bool Loaded() { IntPtr t = Target(); return t != IntPtr.Zero && SendMessage(t, WM_USER, (IntPtr)IS_DB_LOADED, IntPtr.Zero) != IntPtr.Zero; }
  // Indexing, sorting or saving (everything_ipc.h's IS_DB_BUSY).
  public bool Busy() { IntPtr t = Target(); return t == IntPtr.Zero || SendMessage(t, WM_USER, (IntPtr)IS_DB_BUSY, IntPtr.Zero) != IntPtr.Zero; }
  public string Version() {
    IntPtr t = Target();
    return SendMessage(t, WM_USER, (IntPtr)GET_MAJOR, IntPtr.Zero) + "." + SendMessage(t, WM_USER, (IntPtr)GET_MINOR, IntPtr.Zero) + "." +
      SendMessage(t, WM_USER, (IntPtr)GET_REVISION, IntPtr.Zero) + "." + SendMessage(t, WM_USER, (IntPtr)GET_BUILD, IntPtr.Zero);
  }

  protected override void WndProc(ref Message m) {
    if (m.Msg == WM_COPYDATA) {
      COPYDATASTRUCT cds = (COPYDATASTRUCT)Marshal.PtrToStructure(m.LParam, typeof(COPYDATASTRUCT));
      if ((int)cds.dwData == REPLY_ID) {
        // EVERYTHING_IPC_LISTW: totfolders, totfiles, totitems, ...
        TotalItems = (uint)Marshal.ReadInt32(cds.lpData, 8);
        got = true;
        m.Result = (IntPtr)1;
        return;
      }
    }
    base.WndProc(ref m);
  }

  // Round trip in milliseconds for one query (first 100 results).
  public double Query(string q) {
    byte[] text = System.Text.Encoding.Unicode.GetBytes(q + "\0");
    int size = 20 + text.Length;
    IntPtr buf = Marshal.AllocHGlobal(size);
    try {
      Marshal.WriteInt32(buf, 0, (int)Handle);
      Marshal.WriteInt32(buf, 4, REPLY_ID);
      Marshal.WriteInt32(buf, 8, 0);     // search flags
      Marshal.WriteInt32(buf, 12, 0);    // offset
      Marshal.WriteInt32(buf, 16, 100);  // max results
      Marshal.Copy(text, 0, buf + 20, text.Length);
      COPYDATASTRUCT cds = new COPYDATASTRUCT { dwData = (IntPtr)COPYDATAQUERYW, cbData = size, lpData = buf };
      got = false;
      Stopwatch sw = Stopwatch.StartNew();
      if (SendMessage(Target(), WM_COPYDATA, Handle, ref cds) == IntPtr.Zero) throw new Exception("Everything refused the query");
      while (!got) {
        Application.DoEvents();
        if (sw.ElapsedMilliseconds > 60000) throw new TimeoutException(q);
      }
      return sw.Elapsed.TotalMilliseconds;
    } finally { Marshal.FreeHGlobal(buf); }
  }
}
"@

function Median([double[]]$v) { $s = $v | Sort-Object; $n = $s.Count; if ($n % 2) { $s[[int][math]::Floor($n / 2)] } else { ($s[$n / 2 - 1] + $s[$n / 2]) / 2 } }
# Nearest-rank percentile, as ipc_bench computes it.
function Pct([double[]]$v, [double]$p) { $s = @($v | Sort-Object); $s[[int][math]::Round(($s.Count - 1) * $p / 100)] }
function Spread($lo, $hi) { "{0:0.0}-{1:0.0}" -f $lo, $hi }
function Mb($bytes) { [math]::Round($bytes / 1MB, 0) }

$ev = $null
$evStarted = $null      # the Everything process this script started for the queries
$coldProc = $null       # the separate Everything instance of the cold build
try {
  # ---- the machine ---------------------------------------------------------------------
  $cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
  $ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 0)
  $os = (Get-CimInstance Win32_OperatingSystem)
  $disks = (Get-PhysicalDisk | ForEach-Object { "$($_.MediaType) $($_.BusType) $([math]::Round($_.Size / 1GB))GB" }) -join "; "
  Say "machine: $cpu, $ram GB RAM, $($os.Caption) $($os.Version), disks: $disks"

  # ---- rapidfind: wait until every volume is ready (no rescan running) -----------------
  $deadline = (Get-Date).AddMinutes($WaitMinutes)
  $lastSaid = $null
  do {
    $status = @((Run-Step "rapidfind status" $rapidfind @("status") 30).Out)
    $busy = @($status | Where-Object { $_ -match '\d+ entries\s+(\S.*)$' -and $Matches[1] -notmatch '^ready' })
    $up = @($status | Where-Object { $_ -match 'pipe .*: ready' }).Count -gt 0
    if ($up -and $busy.Count -eq 0) { break }
    if (-not $lastSaid -or ((Get-Date) - $lastSaid).TotalSeconds -ge 60) {
      Say "waiting for rapidfind: $(if ($up) { ($busy | ForEach-Object { ($_ -replace '\s+', ' ').Trim() }) -join '; ' } else { 'service not answering' })"
      $lastSaid = Get-Date
    }
    Start-Sleep 5
  } while ((Get-Date) -lt $deadline)
  if ((Get-Date) -ge $deadline) { Say "rapidfind wasn't ready after $WaitMinutes minutes; stopping"; exit 1 }
  $rfBuild = ($status | Select-String "service ([0-9.]+\+[0-9a-f]+)").Matches[0].Groups[1].Value
  $entries = ($status | Select-String "(\d+) entries" -AllMatches | ForEach-Object { $_.Matches } | ForEach-Object { [long]$_.Groups[1].Value } | Measure-Object -Sum).Sum
  Say "rapidfind: service $rfBuild, $entries entries, every volume ready"

  # ---- Everything ----------------------------------------------------------------------
  $ev = New-Object EverythingIpc
  if (-not $ev.Running()) {
    $evStarted = Start-Process $Everything -ArgumentList "-startup" -PassThru
    for ($i = 0; $i -lt 600 -and -not $ev.Running(); $i++) { Start-Sleep -Milliseconds 100 }
    Say "started Everything (process $($evStarted.Id)); it will be closed at the end"
  } else {
    Say "Everything was already running; it stays running"
  }
  $t = [Diagnostics.Stopwatch]::StartNew()
  while (-not $ev.Loaded()) {
    if ($t.Elapsed.TotalSeconds -gt 600) { Say "Everything didn't load its database in 10 minutes; stopping"; exit 1 }
    Start-Sleep -Milliseconds 200
  }
  try { $ev.Query("") | Out-Null } catch { Say "Everything didn't answer a first query: $($_.Exception.Message)"; exit 1 }
  Say "Everything: $($ev.Version()), $($ev.TotalItems) entries (database loaded after $([math]::Round($t.Elapsed.TotalSeconds, 1)) s)"

  # ---- queries -------------------------------------------------------------------------
  $rf = @{}
  Run-Bench (@("--runs", "$Runs") + $Queries) 900 | ForEach-Object { $o = $_ | ConvertFrom-Json; $rf[$o.query] = $o }
  # Everything's own exclude list (its settings), so hit counts can be compared
  # on the same files: rapidfind's hits outside those folders. Counts only.
  $evIni = Join-Path $env:APPDATA "Everything\Everything.ini"
  $excl = @()
  if (Test-Path $evIni) {
    $line = Select-String -Path $evIni -Pattern '^exclude_folders=(.*)$' | Select-Object -First 1
    if ($line) { $excl = [regex]::Matches($line.Matches[0].Groups[1].Value, '"([^"]*)"') | ForEach-Object { $_.Groups[1].Value -replace '\\\\', '\' } }
  }
  $rfOut = @{}
  if ($excl) {
    $not = ($excl | ForEach-Object { "!`"$($_.TrimEnd('\'))\`"" }) -join " "
    foreach ($q in $Queries) {
      $o = Run-Bench @("--runs", "1", "$q $not") 120 | ConvertFrom-Json
      $rfOut[$q] = $o.total
    }
    Say "Everything excludes $($excl.Count) folder(s) in its settings; 'rapidfind hits outside them' counts rapidfind's hits elsewhere"
  }
  $rows = foreach ($q in $Queries) {
    $med = $null; $evSpread = $null
    try {
      $ev.Query($q) | Out-Null   # warm-up
      $times = 1..$Runs | ForEach-Object { $ev.Query($q) }
      $med = [math]::Round((Median $times), 1)
      $evSpread = Spread (Pct $times 10) (Pct $times 90)
    } catch { Say "  Everything: no answer to $q ($($_.Exception.Message))" }
    [pscustomobject]@{
      Query = $q
      "rapidfind ms" = if ($rf[$q]) { [math]::Round($rf[$q].median_ms, 1) } else { $null }
      "p10-p90" = if ($rf[$q]) { Spread $rf[$q].p10_ms $rf[$q].p90_ms } else { $null }
      "Everything ms" = $med
      "p10-p90 " = $evSpread
      "rapidfind hits" = $rf[$q].total
      "outside its excludes" = if ($excl) { $rfOut[$q] } else { $rf[$q].total }
      "Everything hits" = $ev.TotalItems
    }
  }
  Say "median round trip over $Runs runs, and the 10th-90th percentile"
  Say ($rows | Format-Table -AutoSize | Out-String -Width 200)

  # ---- memory and index size -----------------------------------------------------------
  Start-Sleep 5
  # Everything can run a small helper process next to the one holding the database.
  $evp = Get-Process Everything -ErrorAction SilentlyContinue | Sort-Object WorkingSet64 -Descending | Select-Object -First 1
  $rfp = Get-Process rapidfind -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq 0 } | Select-Object -First 1
  Say "memory (working set / private): rapidfind service $(Mb $rfp.WorkingSet64) / $(Mb $rfp.PrivateMemorySize64) MB, Everything $(Mb $evp.WorkingSet64) / $(Mb $evp.PrivateMemorySize64) MB"
  $evDb = Get-Item (Join-Path $env:LOCALAPPDATA "Everything\Everything.db"), (Join-Path $env:APPDATA "Everything\Everything.db") -ErrorAction SilentlyContinue | Select-Object -First 1
  $rfDb = (Get-ChildItem "$env:ProgramData\rapidfind\index" -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
  if (-not $rfDb) {
    # The service's folder is admin-only; the index knows its own size.
    $rfDb = ((Run-Step "rapidfind search" $rapidfind @("search", "-n", "0", "-l", "`"$env:ProgramData\rapidfind\index\ *.db`"") 60).Out |
      ForEach-Object { if ("$_" -match "^\s*([\d.]+) (B|KB|MB|GB)\s") { [double]$Matches[1] * @{ B = 1; KB = 1KB; MB = 1MB; GB = 1GB }[$Matches[2]] } } | Measure-Object -Sum).Sum
  }
  Say "index on disk: rapidfind $(Mb $rfDb) MB, Everything $(Mb $evDb.Length) MB"

  # ---- cold build (elevated) -----------------------------------------------------------
  if ($ColdBuild) {
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) { Say "cold build: skipped (run elevated)" } else {
      # The local volumes Everything includes, from its settings.
      $evVols = @()
      if (Test-Path $evIni) {
        $ini = Get-Content $evIni
        $paths = @(([regex]::Matches((($ini | Where-Object { $_ -match '^ntfs_volume_paths=' }) -replace '^[^=]*=', ''), '"([^"]*)"')) | ForEach-Object { $_.Groups[1].Value })
        $incl = @((($ini | Where-Object { $_ -match '^ntfs_volume_includes=' }) -replace '^[^=]*=', '') -split ',')
        $evVols = @(for ($k = 0; $k -lt $paths.Count; $k++) {
          if ($incl[$k] -eq '1' -and $paths[$k] -match '^([A-Za-z]):$' -and (Test-Path "$($paths[$k])\")) { $Matches[1].ToUpper() }
        }) | Select-Object -Unique
      }
      $vols = if ($ColdBuild -eq "all") { @($evVols) } else { @($ColdBuild -split '[, ]+' | Where-Object { $_ } | ForEach-Object { $_.Substring(0, 1).ToUpper() }) }
      if (-not $vols) { Say "cold build: no volumes (Everything's settings name none)"; $vols = @("C") }
      # In its own process from the MFT, ignoring any saved index (rewrites the
      # per-user local index, not the service's). Volumes build in parallel.
      $label = ($vols | ForEach-Object { "${_}:" }) -join " "
      Say "cold build of ${label}: rapidfind scanning..."
      $argv = @("scan", "--local", "--force") + @($vols | ForEach-Object { "-v"; $_ })
      $r = Run-Step "rapidfind scan" $rapidfind $argv 900
      $r.Err | Where-Object { $_ -match "entries|saved in|done in" } | ForEach-Object { Say ("  " + ($_ -replace '\s*\(index saved to .*\)', '')) }
      Say "cold build of ${label}: rapidfind $($r.Seconds) s (exit code $($r.Code))"

      # Everything: a separate instance, its own database, -reindex, with a
      # copy of its settings (as they are, then with only C: included) passed
      # by -config, so no other settings file is picked up. "Loaded" alone
      # comes too early (it reports a database while volumes are still being
      # indexed): it's done when it's loaded, not busy, and its entry count
      # has held still for 30 s (reading a volume can pause it that long);
      # the time is when the count last changed. Each run also shows the
      # normal database's count, so a short one stands out.
      $tmp = Join-Path $root "target\bench-cold"
      function Cold-Everything([string]$what, [string]$config, $expect) {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue; New-Item -ItemType Directory $tmp | Out-Null
        $cold = New-Object EverythingIpc
        $cold.Class = "EVERYTHING_TASKBAR_NOTIFICATION_(bench)"
        Say "cold build of ${what}: Everything (instance 'bench', its own database) reindexing..."
        $t = [Diagnostics.Stopwatch]::StartNew()
        $script:coldProc = Start-Process $Everything -ArgumentList "-instance bench -startup -config `"$config`" -db `"$(Join-Path $tmp 'everything.db')`" -reindex" -PassThru
        $n = -1; $changed = 0.0; $done = $false; $said = 0.0
        while ($t.Elapsed.TotalSeconds -lt 1200) {
          Start-Sleep -Milliseconds 250
          $now = $t.Elapsed.TotalSeconds
          if (-not $cold.Loaded() -or $cold.Busy()) { $changed = $now; continue }
          $m = $null; try { $cold.Query("") | Out-Null; $m = [long]$cold.TotalItems } catch { $changed = $now; continue }
          if ($m -ne $n) { $n = $m; $changed = $now }
          elseif ($n -gt 0 -and $now - $changed -ge 30) { $done = $true; break }
          if ($now - $said -ge 30) { Say "  ... $([math]::Round($now)) s, $n entries so far"; $said = $now }
        }
        if ($done) {
          $vs = if ($expect) { ", against $expect in its normal database" } else { "" }
          Say "cold build of ${what}: Everything $([math]::Round($changed, 1)) s, $n entries$vs"
        } else { Say "cold build of ${what}: Everything not done after 1200 s ($n entries)" }
        $cold.Dispose()
        & $Everything -instance bench -exit 2>$null
        if (-not $script:coldProc.WaitForExit(60000)) { try { Stop-Process -Id $script:coldProc.Id -Force -ErrorAction Stop } catch { } }
        $script:coldProc = $null
      }
      if (-not (Test-Path $evIni)) { Say "cold build: Everything skipped (no settings file at $evIni)" } else {
        # Expected counts from the normal database: everything, and C: alone.
        $ev.Query("") | Out-Null; $all = $ev.TotalItems
        $ev.Query("c:" + [char]92) | Out-Null; $cOnly = $ev.TotalItems
        $asIs = Join-Path $env:TEMP "everything-bench-all.ini"
        Copy-Item $evIni $asIs -Force
        Cold-Everything "every volume it includes ($(($evVols | ForEach-Object { "${_}:" }) -join ' '))" $asIs $all
        Remove-Item $asIs -ErrorAction SilentlyContinue
        # Its settings with every volume but C: left out (and no folder indexes).
        $onlyC = Join-Path $env:TEMP "everything-bench-c.ini"
        $paths = @(([regex]::Matches((($ini | Where-Object { $_ -match '^ntfs_volume_paths=' }) -replace '^[^=]*=', ''), '"([^"]*)"')) | ForEach-Object { $_.Groups[1].Value })
        $inclC = ($paths | ForEach-Object { if ($_ -eq "C:") { "1" } else { "0" } }) -join ","
        $ini | ForEach-Object {
          if ($_ -match '^ntfs_volume_includes=') { "ntfs_volume_includes=$inclC" }
          elseif ($_ -match '^folders=') { "folders=" }
          else { $_ }
        } | Set-Content -Path $onlyC -Encoding UTF8
        Cold-Everything "C: alone" $onlyC $cOnly
        Remove-Item $onlyC -ErrorAction SilentlyContinue
      }
    }
  }
} finally {
  # Close only what this script started: the cold-build instance by its own
  # name, and the Everything it started for the queries. Never the user's
  # Everything or its (elevated) service.
  if ($script:coldProc) {
    & $Everything -instance bench -exit 2>$null
    if (-not $script:coldProc.WaitForExit(10000)) { try { Stop-Process -Id $script:coldProc.Id -Force -ErrorAction Stop } catch { Say "couldn't close the cold-build Everything (process $($script:coldProc.Id))" } }
  }
  Remove-Item -Recurse -Force (Join-Path $root "target\bench-cold") -ErrorAction SilentlyContinue
  if ($evStarted) {
    & $Everything -exit 2>$null
    if (-not $evStarted.WaitForExit(10000)) { try { Stop-Process -Id $evStarted.Id -Force -ErrorAction Stop } catch { Say "couldn't close the Everything this script started (process $($evStarted.Id))" } }
  }
  if ($ev) { $ev.Dispose() }
  Say ("bench.ps1 finished {0:yyyy-MM-dd HH:mm:ss}" -f (Get-Date))
}
