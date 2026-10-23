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
param(
  [int]$Runs = 15,
  [string[]]$Queries = @("report", "*.pdf", "ext:dll", "size:>1gb", "dm:today", "c:\windows\system32\*.dll", "*\system32\*.dll", "f:\ *.jpg", "a", "f:\"),
  [string]$ColdBuild = "",
  [string]$Everything = "C:\Program Files\Everything\Everything.exe"
)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..\..").Path
$bench = Join-Path $root "target\release\examples\ipc_bench.exe"
if (-not (Test-Path $bench)) {
  Push-Location $root
  $ErrorActionPreference = "Continue"
  & cargo build --release -p rapidfind-core --example ipc_bench 2>&1 | Where-Object { "$_" -match "^error" }
  $ErrorActionPreference = "Stop"
  Pop-Location
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

# ---- the machine ---------------------------------------------------------------------
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 0)
$os = (Get-CimInstance Win32_OperatingSystem)
$disks = (Get-PhysicalDisk | ForEach-Object { "$($_.MediaType) $($_.BusType) $([math]::Round($_.Size / 1GB))GB" }) -join "; "
"machine: $cpu, $ram GB RAM, $($os.Caption) $($os.Version), disks: $disks"
$status = & (Join-Path $root "target\release\rapidfind.exe") status 2>&1 | ForEach-Object { "$_" }
$rfBuild = ($status | Select-String "service ([0-9.]+\+[0-9a-f]+)").Matches[0].Groups[1].Value
$entries = ($status | Select-String "(\d+) entries" -AllMatches | ForEach-Object { $_.Matches } | ForEach-Object { [long]$_.Groups[1].Value } | Measure-Object -Sum).Sum
"rapidfind: service $rfBuild, $entries entries"

# ---- Everything ----------------------------------------------------------------------
$ev = New-Object EverythingIpc
$started = $false
if (-not $ev.Running()) {
  Start-Process $Everything -ArgumentList "-startup" | Out-Null
  $started = $true
  for ($i = 0; $i -lt 600 -and -not $ev.Running(); $i++) { Start-Sleep -Milliseconds 100 }
}
$t = [Diagnostics.Stopwatch]::StartNew()
while (-not $ev.Loaded()) { if ($t.Elapsed.TotalSeconds -gt 300) { throw "Everything didn't load its database" }; Start-Sleep -Milliseconds 100 }
$ev.Query("") | Out-Null
"Everything: $($ev.Version()), $($ev.TotalItems) entries (loaded its database in $([math]::Round($t.Elapsed.TotalSeconds, 1)) s)"

# ---- queries -------------------------------------------------------------------------
$rf = @{}
& $bench --runs $Runs @Queries | ForEach-Object { $o = $_ | ConvertFrom-Json; $rf[$o.query] = $o }
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
    $o = & $bench --runs 1 "$q $not" | ConvertFrom-Json
    $rfOut[$q] = $o.total
  }
  "Everything excludes $($excl.Count) folder(s) in its settings; 'rapidfind hits outside them' counts rapidfind's hits elsewhere"
}
$rows = foreach ($q in $Queries) {
  $ev.Query($q) | Out-Null   # warm-up
  $times = 1..$Runs | ForEach-Object { $ev.Query($q) }
  [pscustomobject]@{
    Query = $q
    "rapidfind ms" = [math]::Round($rf[$q].median_ms, 1)
    "rapidfind hits" = $rf[$q].total
    "rapidfind hits outside them" = if ($excl) { $rfOut[$q] } else { $rf[$q].total }
    "Everything ms" = [math]::Round((Median $times), 1)
    "Everything hits" = $ev.TotalItems
  }
}
$rows | Format-Table -AutoSize | Out-String -Width 200

# ---- memory and index size -----------------------------------------------------------
Start-Sleep 5
# Everything can run a small helper process next to the one holding the database.
$evp = Get-Process Everything | Sort-Object WorkingSet64 -Descending | Select-Object -First 1
$rfp = Get-Process rapidfind | Where-Object { $_.SessionId -eq 0 } | Select-Object -First 1
"memory (working set / private): rapidfind service $(Mb $rfp.WorkingSet64) / $(Mb $rfp.PrivateMemorySize64) MB, Everything $(Mb $evp.WorkingSet64) / $(Mb $evp.PrivateMemorySize64) MB"
$evDb = Get-Item (Join-Path $env:LOCALAPPDATA "Everything\Everything.db"), (Join-Path $env:APPDATA "Everything\Everything.db") -ErrorAction SilentlyContinue | Select-Object -First 1
try { $rfDb = (Get-ChildItem "$env:ProgramData\rapidfind\index" -File -ErrorAction Stop | Measure-Object Length -Sum).Sum }
catch { $rfDb = $null }   # the service's folder is admin-only; the index knows its own size
if (-not $rfDb) {
  $ErrorActionPreference = "Continue"   # the CLI reports its count on stderr
  $rfDb =(& (Join-Path $root "target\release\rapidfind.exe") search -n 0 -l "$env:ProgramData\rapidfind\index\ *.db" 2>$null |
    ForEach-Object { if ("$_" -match "^\s*([\d.]+) (B|KB|MB|GB)\s") { [double]$Matches[1] * @{ B = 1; KB = 1KB; MB = 1MB; GB = 1GB }[$Matches[2]] } } | Measure-Object -Sum).Sum
}
"index on disk: rapidfind $(Mb $rfDb) MB, Everything $(Mb $evDb.Length) MB"

# ---- cold build (elevated) -----------------------------------------------------------
if ($ColdBuild) {
  $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  if (-not $admin) { "cold build: skipped (run elevated)" } else {
    $tmp = Join-Path $root "target\bench-cold"
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue; New-Item -ItemType Directory $tmp | Out-Null
    # In this process from the MFT, ignoring any saved index (rewrites the per-user local index, not the service's).
    $t = [Diagnostics.Stopwatch]::StartNew()
    & (Join-Path $root "target\release\rapidfind.exe") scan --local --force -v $ColdBuild 2>&1 | Out-Null
    "cold build of ${ColdBuild}: rapidfind $([math]::Round($t.Elapsed.TotalSeconds, 1)) s"
    $inst = Join-Path $tmp "everything.db"
    $cold = New-Object EverythingIpc
    $cold.Class = "EVERYTHING_TASKBAR_NOTIFICATION_(bench)"
    $t = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process $Everything -ArgumentList "-instance bench -startup -db `"$inst`" -reindex" -PassThru
    while (-not $cold.Loaded()) { if ($t.Elapsed.TotalSeconds -gt 900) { break }; Start-Sleep -Milliseconds 100 }
    "cold build of every included volume: Everything $([math]::Round($t.Elapsed.TotalSeconds, 1)) s (its -db/-reindex rebuilds all its volumes, not one)"
    Stop-Process -Id $p.Id -Force
  }
}
# Close the Everything this script started (its own -exit; its service, which
# runs elevated, isn't ours to stop).
if ($started) {
  & $Everything -exit 2>$null
  Start-Sleep 2
  Get-Process Everything -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 -and $_.Path } |
    ForEach-Object { try { Stop-Process -Id $_.Id -Force -ErrorAction Stop } catch { } }
}
$ev.Dispose()
