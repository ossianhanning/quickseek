# rapidfind vs Everything on this PC: query latency, memory, index size and
# (elevated only) a cold index build. Prints numbers only, no file names.
#
#   tools\bench\bench.ps1                     queries, memory, index size
#   tools\bench\bench.ps1 -Runs 25            more runs per query
#   tools\bench\bench.ps1 -ColdBuild C        also time a cold build of C: (run elevated)
#
# Query latency is what a client sees: a round trip to each tool's own
# process over its own IPC (rapidfind's pipe, Everything's WM_COPYDATA
# query), sorted by name, first 100 results. One warm-up run, then the
# median of -Runs. rapidfind needs its service running; Everything is
# started (tray, no window) if it isn't, and loads its existing database.
#
# Every line also goes to -Log as it happens (default target\tmp\bench-<time>.txt).
# It waits for rapidfind to finish any background rescan first, every step
# has a time limit, and it only closes the Everything instances it started.
param(
  [int]$Runs = 15,
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
  const int WM_COPYDATA = 0x4A, WM_USER = 0x400, IS_DB_LOADED = 401, GET_MAJOR = 0, GET_MINOR = 1, GET_REVISION = 2, GET_BUILD = 3;
  const int COPYDATAQUERYW = 2, REPLY_ID = 0x52464231;
  public string Class = "EVERYTHING_TASKBAR_NOTIFICATION";
  bool got; public uint TotalItems;

  public EverythingIpc() { CreateHandle(new CreateParams { Parent = (IntPtr)(-3) }); }
  public void Dispose() { DestroyHandle(); }
  IntPtr Target() { return FindWindow(Class, null); }
  public bool Running() { return Target() != IntPtr.Zero; }
  public bool Loaded() { IntPtr t = Target(); return t != IntPtr.Zero && SendMessage(t, WM_USER, (IntPtr)IS_DB_LOADED, IntPtr.Zero) != IntPtr.Zero; }
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
    $med = $null
    try {
      $ev.Query($q) | Out-Null   # warm-up
      $times = 1..$Runs | ForEach-Object { $ev.Query($q) }
      $med = [math]::Round((Median $times), 1)
    } catch { Say "  Everything: no answer to $q ($($_.Exception.Message))" }
    [pscustomobject]@{
      Query = $q
      "rapidfind ms" = if ($rf[$q]) { [math]::Round($rf[$q].median_ms, 1) } else { $null }
      "rapidfind hits" = $rf[$q].total
      "rapidfind hits outside them" = if ($excl) { $rfOut[$q] } else { $rf[$q].total }
      "Everything ms" = $med
      "Everything hits" = $ev.TotalItems
    }
  }
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
      # In its own process from the MFT, ignoring any saved index (rewrites the
      # per-user local index, not the service's).
      Say "cold build of ${ColdBuild}: rapidfind scanning..."
      $r = Run-Step "rapidfind scan" $rapidfind @("scan", "--local", "--force", "-v", $ColdBuild) 900
      $r.Err | Where-Object { $_ -match "entries|saved in|done in" } | ForEach-Object { Say ("  " + ($_ -replace '\s*\(index saved to .*\)', '')) }
      Say "cold build of ${ColdBuild}: rapidfind $($r.Seconds) s (exit code $($r.Code))"
      $tmp = Join-Path $root "target\bench-cold"
      Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue; New-Item -ItemType Directory $tmp | Out-Null
      $cold = New-Object EverythingIpc
      $cold.Class = "EVERYTHING_TASKBAR_NOTIFICATION_(bench)"
      Say "cold build: Everything (instance 'bench', its own database) reindexing..."
      $t = [Diagnostics.Stopwatch]::StartNew()
      $coldProc = Start-Process $Everything -ArgumentList "-instance bench -startup -db `"$(Join-Path $tmp 'everything.db')`" -reindex" -PassThru
      $done = $false
      while ($t.Elapsed.TotalSeconds -lt 900) { if ($cold.Loaded()) { $done = $true; break }; Start-Sleep -Milliseconds 100 }
      if ($done) { Say "cold build of every included volume: Everything $([math]::Round($t.Elapsed.TotalSeconds, 1)) s (its -db/-reindex rebuilds all its volumes, not one)" }
      else { Say "cold build: Everything not done after 900 s" }
      $cold.Dispose()
    }
  }
} finally {
  # Close only what this script started: the cold-build instance by its own
  # name, and the Everything it started for the queries. Never the user's
  # Everything or its (elevated) service.
  if ($coldProc) {
    & $Everything -instance bench -exit 2>$null
    if (-not $coldProc.WaitForExit(10000)) { try { Stop-Process -Id $coldProc.Id -Force -ErrorAction Stop } catch { Say "couldn't close the cold-build Everything (process $($coldProc.Id))" } }
    Remove-Item -Recurse -Force (Join-Path $root "target\bench-cold") -ErrorAction SilentlyContinue
  }
  if ($evStarted) {
    & $Everything -exit 2>$null
    if (-not $evStarted.WaitForExit(10000)) { try { Stop-Process -Id $evStarted.Id -Force -ErrorAction Stop } catch { Say "couldn't close the Everything this script started (process $($evStarted.Id))" } }
  }
  if ($ev) { $ev.Dispose() }
  Say ("bench.ps1 finished {0:yyyy-MM-dd HH:mm:ss}" -f (Get-Date))
}
