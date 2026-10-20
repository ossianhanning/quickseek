# Starts the mock service and the GUI (debug builds) against it, optionally
# sets a query and/or opens an Options tab, then takes a screenshot.
#   tools\gui-mock.ps1 -Query "mp3|Documents"
#   tools\gui-mock.ps1 -OptionsTab 4 -Out target\opt4.png
param([string]$Query = "", [int]$OptionsTab = -1, [string]$Out = "$PSScriptRoot\..\target\shot.png")
$t = "$PSScriptRoot\..\target"
Get-Process rapidfind-gui -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "*\debug\*" } | Stop-Process -Force
if (-not (Get-Process mock_service -ErrorAction SilentlyContinue)) {
  Start-Process "$t\debug\examples\mock_service.exe" -ArgumentList "--live" -WindowStyle Hidden | Out-Null
  Start-Sleep 3
}
$env:RAPIDFIND_PIPE = '\\.\pipe\rapidfind-mock'
$env:RAPIDFIND_GUI_DIR = "$t\gui-test-settings"
Start-Process "$t\debug\rapidfind-gui.exe" -ArgumentList "--test" | Out-Null
Start-Sleep 2
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class RfMock {
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, IntPtr t);
 [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h, int id);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, string l);
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
"@
$h = [RfMock]::FindWindow("rapidfind.main", [IntPtr]::Zero)
[RfMock]::SendMessage([RfMock]::GetDlgItem($h, 100), 0x0C, [IntPtr]::Zero, $Query) | Out-Null
Start-Sleep 2
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
