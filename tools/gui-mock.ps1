# Starts the mock service and the GUI (debug builds) against it, optionally
# sets a query and/or opens an Options tab, then takes a screenshot.
#   tools\gui-mock.ps1 -Query "mp3|Documents"
#   tools\gui-mock.ps1 -OptionsTab 4 -Out target\opt4.png
#   tools\gui-mock.ps1 -Command 403          (a menu command, e.g. 403 = folder size tree)
#   tools\gui-mock.ps1 -Command 403,411 -PaneTab 2   (compare with a week ago, growth pane's Map tab)
param([string]$Query = "", [int]$OptionsTab = -1, [int[]]$Command = @(), [int]$PaneTab = -1, [switch]$RestartMock, [switch]$Live, [string]$Out = "$PSScriptRoot\..\target\shot.png")
$t = "$PSScriptRoot\..\target"
Get-Process rapidfind-gui -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "*\debug\*" } | ForEach-Object { $_ | Stop-Process -Force; $_.WaitForExit(5000) | Out-Null }
if ($RestartMock) { Get-Process mock_service -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 1 }
if (-not (Get-Process mock_service -ErrorAction SilentlyContinue)) {
  Start-Process "$t\debug\examples\mock_service.exe" -ArgumentList $(if ($Live) { "--live" } else { "--pipe \\.\pipe\rapidfind-mock" }) -WindowStyle Hidden | Out-Null
  for ($i = 0; $i -lt 100 -and -not (Test-Path '\\.\pipe\rapidfind-mock'); $i++) { Start-Sleep -Milliseconds 200 }
}
$env:RAPIDFIND_PIPE = '\\.\pipe\rapidfind-mock'
$env:RAPIDFIND_GUI_DIR = "$t\gui-test-settings"
$gui = Start-Process "$t\debug\rapidfind-gui.exe" -ArgumentList "--test" -PassThru
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class RfMock {
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, IntPtr t);
 [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h, int id);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, string l);
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
 [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
}
"@
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 75 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 200; $h = [RfMock]::FindWindow("rapidfind.main", [IntPtr]::Zero) }
if ($h -eq [IntPtr]::Zero) { "window not found (gui exited: $($gui.HasExited), code $($gui.ExitCode))"; exit 1 }
Start-Sleep 2
foreach ($c in $Command) { [RfMock]::PostMessage($h, 0x111, [IntPtr]$c, [IntPtr]::Zero) | Out-Null; Start-Sleep 1 }
if ($Query) { [RfMock]::SendMessage([RfMock]::GetDlgItem($h, 100), 0x0C, [IntPtr]::Zero, $Query) | Out-Null }
Start-Sleep 2
if ($PaneTab -ge 0) {
  # Click the tab (growth.rs pane_tabs: 12 px margins, weights 30/17/16/23, 56 px down).
  [RfMock]::SetProcessDPIAware() | Out-Null; $pane = [RfMock]::GetDlgItem($h, 107); $r = New-Object RfMock+RECT; [RfMock]::GetClientRect($pane, [ref]$r) | Out-Null
  $s = [RfMock]::GetDpiForWindow($pane) / 96.0; $w = $r.R - 24 * $s; $starts = @(0, 30, 47, 63, 86)
  $x = [int](12 * $s + $w * ($starts[$PaneTab] + $starts[$PaneTab + 1]) / 2 / 86); $y = [int](67 * $s)
  [RfMock]::PostMessage($pane, 0x201, [IntPtr]1, [IntPtr](($y -shl 16) -bor $x)) | Out-Null
  Start-Sleep 2
}
if ($OptionsTab -ge 0) {
  [RfMock]::PostMessage($h, 0x111, [IntPtr]500, [IntPtr]::Zero) | Out-Null   # Tools > Options
  Start-Sleep 2
  $o = [RfMock]::FindWindow("rapidfind.options", [IntPtr]::Zero)
  [RfMock]::PostMessage($o, 0x111, [IntPtr](980 + $OptionsTab), [IntPtr]::Zero) | Out-Null   # select page
  Start-Sleep -Milliseconds 500
  & "$PSScriptRoot\shot.ps1" -ClassName "rapidfind.options" -Out $Out
} else {
  & "$PSScriptRoot\shot.ps1" -Out $Out
}
