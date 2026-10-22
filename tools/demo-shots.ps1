# Screenshots of the GUI against the demo PC (crates\gui\tests\common\demo.rs),
# never this machine's files. Builds the release GUI and demo service if
# needed, starts the service, and captures each shot into docs\images:
#   tools\demo-shots.ps1                 all shots
#   tools\demo-shots.ps1 -Only search    one of them
# Shots are taken at this display's scale (name.png) and also scaled to
# 96 DPI (name@1x.png), as optimized PNGs when optipng or oxipng is on PATH.
param([string]$Only = "", [switch]$NoBuild)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").Path
$rel = Join-Path $root "target\release"
$out = Join-Path $root "docs\images"
New-Item -ItemType Directory -Force $out | Out-Null

if (-not $NoBuild) {
  Push-Location $root
  $ErrorActionPreference = "Continue"
  & cargo build --release -p rapidfind-gui --bin rapidfind-gui --example demo_service 2>&1 | Where-Object { "$_" -match "^(error|warning)" }
  $ErrorActionPreference = "Stop"
  Pop-Location
}

$pipe = '\\.\pipe\rapidfind-demo'
if (-not (Test-Path $pipe)) {
  Start-Process "$rel\examples\demo_service.exe" -WindowStyle Hidden | Out-Null
  for ($i = 0; $i -lt 150 -and -not (Test-Path $pipe); $i++) { Start-Sleep -Milliseconds 200 }
  Start-Sleep 1
}

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class RfDemo {
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, IntPtr t);
 [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h, int id);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, string l);
 [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
 [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
 [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
[RfDemo]::SetProcessDPIAware() | Out-Null

function Save-Png($bmp, $path) {
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  foreach ($tool in "oxipng", "optipng") {
    if (Get-Command $tool -ErrorAction SilentlyContinue) {
      if ($tool -eq "oxipng") { & oxipng -o 4 --strip safe -q $path } else { & optipng -quiet -o5 $path }
      break
    }
  }
}

function Capture($h, $name, [switch]$NoOneX) {
  $r = New-Object RfDemo+RECT
  [RfDemo]::GetWindowRect($h, [ref]$r) | Out-Null
  $w = $r.R - $r.L; $hh = $r.B - $r.T
  $bmp = New-Object System.Drawing.Bitmap $w, $hh
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $dc = $g.GetHdc(); [RfDemo]::PrintWindow($h, $dc, 2) | Out-Null; $g.ReleaseHdc($dc); $g.Dispose()
  # PrintWindow includes the invisible resize borders of Windows 10/11: trim them.
  $s = [RfDemo]::GetDpiForWindow($h) / 96.0
  $trim = [int](7 * $s)
  $crop = $bmp.Clone((New-Object System.Drawing.Rectangle $trim, 0, ($w - 2 * $trim), ($hh - $trim)), $bmp.PixelFormat)
  $dest = if ($name -like "frames\*") { Join-Path $root "target\demo-gui\$name.png" } else { Join-Path $out "$name.png" }
  if ($NoOneX) { $crop.Save($dest, [System.Drawing.Imaging.ImageFormat]::Png); $crop.Dispose(); $bmp.Dispose(); return }
  Save-Png $crop $dest
  $k = 1.0 / $s
  $small = New-Object System.Drawing.Bitmap ([int]($crop.Width * $k)), ([int]($crop.Height * $k))
  $g = [System.Drawing.Graphics]::FromImage($small)
  $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $g.DrawImage($crop, 0, 0, $small.Width, $small.Height); $g.Dispose()
  Save-Png $small (Join-Path $out "$name@1x.png")
  "$name.png: $($crop.Width) x $($crop.Height) (scale $s), $name@1x.png: $($small.Width) x $($small.Height)"
  $crop.Dispose(); $small.Dispose(); $bmp.Dispose()
}

# One shot: fresh settings, start the GUI, run the steps, capture, close.
function Shot($name, [hashtable]$settings, [scriptblock]$steps) {
  if ($Only -and $Only -ne $name) { return }
  $dir = Join-Path $root "target\demo-gui\$name"
  Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force $dir | Out-Null
  $s = @{ window = @(60, 40, 1920, 1170); sort = "size"; descending = $true }
  foreach ($k in $settings.Keys) { $s[$k] = $settings[$k] }
  # No BOM: the GUI reads plain UTF-8 JSON.
  [IO.File]::WriteAllText((Join-Path $dir "gui.json"), ($s | ConvertTo-Json))
  $env:RAPIDFIND_PIPE = $pipe
  $env:RAPIDFIND_GUI_DIR = $dir
  $gui = Start-Process "$rel\rapidfind-gui.exe" -ArgumentList "--test" -PassThru
  $h = [IntPtr]::Zero
  for ($i = 0; $i -lt 75 -and $h -eq [IntPtr]::Zero; $i++) {
    Start-Sleep -Milliseconds 200
    $h = (Get-Process -Id $gui.Id).MainWindowHandle
  }
  if ($h -eq [IntPtr]::Zero) { throw "the GUI didn't open a window" }
  Start-Sleep 2
  & $steps $h
  Start-Sleep 2
  if ($name -ne "search-typing") { Capture $h $name }
  Stop-Process -Id $gui.Id -Force
  Start-Sleep -Milliseconds 500
}

function Query($h, $text) { [RfDemo]::SendMessage([RfDemo]::GetDlgItem($h, 100), 0x0C, [IntPtr]::Zero, $text) | Out-Null }
function Command($h, [int]$id) { [RfDemo]::PostMessage($h, 0x111, [IntPtr]$id, [IntPtr]::Zero) | Out-Null; Start-Sleep 1 }
function PaneTab($h, [int]$tab) {
  # growth.rs pane_tabs: 12 px margins, tab weights 30/17/16/23, 56 px down.
  $pane = [RfDemo]::GetDlgItem($h, 107); $r = New-Object RfDemo+RECT; [RfDemo]::GetClientRect($pane, [ref]$r) | Out-Null
  $s = [RfDemo]::GetDpiForWindow($pane) / 96.0; $w = $r.R - 24 * $s; $starts = @(0, 30, 47, 63, 86)
  $x = [int](12 * $s + $w * ($starts[$tab] + $starts[$tab + 1]) / 2 / 86); $y = [int](67 * $s)
  [RfDemo]::PostMessage($pane, 0x201, [IntPtr]1, [IntPtr](($y -shl 16) -bor $x)) | Out-Null
  [RfDemo]::PostMessage($pane, 0x202, [IntPtr]0, [IntPtr](($y -shl 16) -bor $x)) | Out-Null
  Start-Sleep 1
}

# The hero animation: typing a search, one frame per keystroke
# (tools\frames-to-anim.py makes docs\images\search-as-you-type.webp).
Shot "search-typing" @{ window = @(60, 40, 1920, 900) } {
  param($h)
  $frames = Join-Path $root "target\demo-gui\frames"
  Remove-Item -Recurse -Force $frames -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force $frames | Out-Null
  $n = 0
  foreach ($q in @("", "i", "ic", "ice", "icel", "icela", "icelan", "iceland", "iceland*", "iceland*\", "iceland*\*", "iceland*\*.", "iceland*\*.m", "iceland*\*.mp", "iceland*\*.mp4")) {
    Query $h $q
    Start-Sleep -Milliseconds 350
    Capture $h ("frames\{0:D2}" -f $n) -NoOneX
    $n++
  }
}
Shot "search" @{ window = @(60, 40, 1920, 1000) } { param($h) Query $h "iceland" }
Shot "tree" @{ tree_mode = $true } { param($h) }
Shot "growth" @{ tree_mode = $true } { param($h) Command $h 412; PaneTab $h 0 }
Shot "growth-map" @{ tree_mode = $true } { param($h) Command $h 412; PaneTab $h 2 }
