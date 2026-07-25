# Regenerate docs/screenshot.png from ccdesk --demo, rendered by Windows Terminal.
#
# Usage (from the repository root, after a release build):
#
#   .\docs\screenshot.ps1 -Exe .\target\release\ccdesk.exe -Out .\docs\screenshot.png
#
# Run this whenever the sidebar or the new-session screen changes shape, so the
# README image keeps matching the current version. Check the result before
# committing: the crop is per-window, but a modal dialog that pops up over the
# terminal during the capture would end up in the image.
#
# WT is used (not conhost) so the image matches how users actually see ccdesk:
# conhost renders thinner glyphs and puts "conhost.exe" in the title bar.
#
# Window identification is the hard part: WT runs every window under ONE process,
# so a new window cannot be found by process. Instead we enumerate top-level WT
# windows BEFORE launching and pick whichever handle is new afterwards. Capturing
# the wrong window would leak the developer's own session into a published image,
# so the script fails loudly rather than guessing.
#
# --size sets the terminal to its final dimensions at launch, so ccdesk lays out
# once and we never resize a running TUI (resizing leaves stale glyphs).
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [Parameter(Mandatory=$true)][string]$Out,
  [int]$Cols = 150,
  [int]$Rows = 34,
  [int]$SettleSec = 6
)

Add-Type -AssemblyName System.Drawing
Add-Type -Namespace Win -Name Api -MemberDefinition @'
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, System.Text.StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
'@

function Get-TerminalWindows {
  $script:acc = New-Object System.Collections.ArrayList
  $cb = [Win.Api+EnumProc]{
    param($h, $p)
    if ([Win.Api]::IsWindowVisible($h)) {
      $sb = New-Object System.Text.StringBuilder 256
      [void][Win.Api]::GetClassNameW($h, $sb, $sb.Capacity)
      # WT's top-level window class; stable across versions.
      if ($sb.ToString() -eq 'CASCADIA_HOSTING_WINDOW_CLASS') { [void]$script:acc.Add($h) }
    }
    return $true
  }
  [void][Win.Api]::EnumWindows($cb, [IntPtr]::Zero)
  return $script:acc
}

Get-Process ccdesk -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

$before = @(Get-TerminalWindows)

Start-Process wt.exe -ArgumentList @(
  '-w','new','--size',"$Cols,$Rows",
  'new-tab','--suppressApplicationTitle','--title','ccdesk',
  '--', $Exe, '--demo'
)

$h = [IntPtr]::Zero
for ($i = 0; $i -lt 60; $i++) {
  Start-Sleep -Milliseconds 250
  $now = @(Get-TerminalWindows)
  $new = $now | Where-Object { $before -notcontains $_ }
  if ($new.Count -eq 1) { $h = $new[0]; break }
  if ($new.Count -gt 1) { throw "more than one new terminal window appeared; refusing to guess" }
}
if ($h -eq [IntPtr]::Zero) { throw "no new terminal window appeared" }

# Let ccdesk spawn its PTY and paint at the launch size. No resizing.
Start-Sleep -Seconds $SettleSec
[void][Win.Api]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 900

$r = New-Object Win.Api+RECT
if ([Win.Api]::DwmGetWindowAttribute($h, 9, [ref]$r, 16) -ne 0) {
  [void][Win.Api]::GetWindowRect($h, [ref]$r)
}
$w = $r.Right - $r.Left
$ht = $r.Bottom - $r.Top
if (($w -le 0) -or ($ht -le 0)) { throw "bad window rect" }

$bmp = New-Object System.Drawing.Bitmap $w, $ht
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $w, $ht))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

Get-Process ccdesk -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

"saved: $Out size=${w}x${ht}"
