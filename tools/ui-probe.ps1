# Development helper: drive and screenshot the running Myuri without global input.
#   -Title   window title to act on ('' is the panel)
#   -Keys    comma separated: letters, digits, esc, enter, down
#   -Click   "x,y" in window pixels
#   -Drag    "x1,y1,x2,y2": press at the first point, move in steps, release at the second
#   -Shot    name of a PNG written to target/
#   -Close   close the window afterwards
param([string]$Title = '', [string]$Keys = '', [string]$Click = '', [string]$Drag = '', [string]$Shot = '', [switch]$Close)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class W {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
[void][W]::SetProcessDPIAware()
$target = (Get-Process myuri -ErrorAction Stop | Select-Object -First 1).Id
$script:wins = @()
[void][W]::EnumWindows({ param($h, $l)
  $p = 0; [void][W]::GetWindowThreadProcessId($h, [ref]$p)
  if ($p -eq $target -and [W]::IsWindowVisible($h)) {
    $r = New-Object W+RECT; [void][W]::GetWindowRect($h, [ref]$r)
    $t = New-Object System.Text.StringBuilder 128; [void][W]::GetWindowTextW($h, $t, 128)
    if (($r.R - $r.L) -gt 50) { $script:wins += ,@($h, $r, $t.ToString()) }
  }
  return $true }, [IntPtr]::Zero)
$vk = @{ esc = 0x1B; enter = 0x0D; down = 0x28 }
$found = $false
foreach ($w in $script:wins) {
  $h, $r, $t = $w
  if ($t -ne $Title) { continue }
  $found = $true
  foreach ($k in $Keys.Split(',', [StringSplitOptions]::RemoveEmptyEntries)) {
    $code = if ($vk.ContainsKey($k)) { $vk[$k] } else { [int][char]$k.ToUpper() }
    [void][W]::PostMessageW($h, 0x100, [IntPtr]$code, [IntPtr]1); [void][W]::PostMessageW($h, 0x101, [IntPtr]$code, [IntPtr]0xC0000001)
    Start-Sleep -Milliseconds 60
  }
  if ($Click) {
    $c = $Click.Split(','); $lp = [IntPtr](([int]$c[1] -shl 16) -bor ([int]$c[0] -band 0xFFFF))
    [void][W]::PostMessageW($h, 0x200, [IntPtr]0, $lp); Start-Sleep -Milliseconds 80
    [void][W]::PostMessageW($h, 0x201, [IntPtr]1, $lp); Start-Sleep -Milliseconds 60
    [void][W]::PostMessageW($h, 0x202, [IntPtr]0, $lp); Start-Sleep -Milliseconds 250
  }
  if ($Drag) {
    $d = $Drag.Split(',') | ForEach-Object { [int]$_ }
    $at = { param($x, $y) [IntPtr]((([int]$y) -shl 16) -bor (([int]$x) -band 0xFFFF)) }
    [void][W]::PostMessageW($h, 0x200, [IntPtr]0, (& $at $d[0] $d[1])); Start-Sleep -Milliseconds 80
    [void][W]::PostMessageW($h, 0x201, [IntPtr]1, (& $at $d[0] $d[1])); Start-Sleep -Milliseconds 80
    foreach ($i in 1..12) {
      $x = $d[0] + ($d[2] - $d[0]) * $i / 12; $y = $d[1] + ($d[3] - $d[1]) * $i / 12
      [void][W]::PostMessageW($h, 0x200, [IntPtr]1, (& $at $x $y)); Start-Sleep -Milliseconds 30
    }
    [void][W]::PostMessageW($h, 0x202, [IntPtr]0, (& $at $d[2] $d[3])); Start-Sleep -Milliseconds 400
  }
  if ($Shot) {
    Start-Sleep -Milliseconds 200
    [void][W]::GetWindowRect($h, [ref]$r)
    $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
    [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
    $bmp.Save((Join-Path $PSScriptRoot "..\target\$Shot.png"))
  }
  if ($Close) { [void][W]::PostMessageW($h, 0x10, [IntPtr]0, [IntPtr]0) }
  "window '$t' $($r.R - $r.L)x$($r.B - $r.T)"
}
if (-not $found) { "no window titled '$Title'; visible: $(($script:wins | ForEach-Object { "'" + $_[2] + "'" }) -join ' ')" }
