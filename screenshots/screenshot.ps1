# Regenerate screenshots/screenshot.png from ccdesk --demo, rendered by Windows Terminal.
#
# The image and the script that produces it live in the same folder on purpose: they
# only make sense together, and splitting them across assets/ and scripts/ meant
# neither directory said what it was for.
#
# Usage (from the repository root, after a release build):
#
#   .\screenshots\screenshot.ps1 -Exe .\target\release\ccdesk.exe -Out .\screenshots\screenshot.png
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
#
# Process cleanup follows the same "never guess" rule. An earlier version closed the
# capture window with `Get-Process ccdesk | Stop-Process`, which killed EVERY running
# ccdesk -- including the developer's own, and this script is typically run from a
# Claude Code session hosted inside ccdesk, so it killed the session that started it.
# The demo process is now identified the same way the window is: snapshot the ccdesk
# pids before launching, and afterwards only touch a pid that is both new AND running
# the exact `<resolved exe> --demo` command line we issued. If nothing matches we warn
# and kill nothing, because leaving a stray demo window open is recoverable while
# killing someone's live session is not.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [Parameter(Mandatory=$true)][string]$Out,
  [int]$Cols = 150,
  [int]$Rows = 34,
  [int]$SettleSec = 6
)

# Resolve the exe up front: the demo process is later recognised by its command line,
# which only matches if wt.exe was handed the same absolute path we compare against.
#
# -ErrorAction Stop is load-bearing. Resolve-Path reports a missing path as a
# NON-terminating error by default, so without it a typo leaves $ExePath as $null and
# the script keeps going (verified): wt.exe is handed a null argument so there is
# nothing to capture, and the cleanup pass compares command lines against that null
# ($_.CommandLine.IndexOf($null, ...) returns -1 for every candidate), so
# Get-DemoProcess matches nothing and the capture window is left open. Fail here
# instead -- that is the only point where a bad -Exe is still cheap to report.
$ExePath = (Resolve-Path -LiteralPath $Exe -ErrorAction Stop).ProviderPath

# Strip color-suppressing variables before launching. This script is typically run
# from a Claude Code session, whose shell sets NO_COLOR=1; the demo ccdesk inherits
# it through wt.exe and crossterm honours it, so every published screenshot came out
# monochrome. CLICOLOR=0 would do the same, so clear both.
Remove-Item Env:NO_COLOR -ErrorAction SilentlyContinue
Remove-Item Env:CLICOLOR -ErrorAction SilentlyContinue

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

# Only processes that appeared after the launch and carry our demo command line. WT
# hosts every window in one process, so the ccdesk we start is a child of the shared
# WindowsTerminal.exe -- the parent pid cannot tell our window from the developer's,
# which is why the command line is the discriminator.
function Get-DemoProcess {
  param([int[]]$ExcludePids)
  return @(Get-CimInstance Win32_Process -Filter "Name='ccdesk.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $ExcludePids -notcontains [int]$_.ProcessId } |
    Where-Object {
      $_.CommandLine -and
      $_.CommandLine.IndexOf($ExePath, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 -and
      $_.CommandLine -match '(^|\s)--demo(\s|$)'
    })
}

# No pre-launch cleanup: a leftover demo window from an aborted run cannot corrupt the
# capture (the window diff below ignores any window that already existed), so there is
# nothing to gain from killing processes this run did not start.
$beforePids = @(Get-Process ccdesk -ErrorAction SilentlyContinue | ForEach-Object { [int]$_.Id })

$before = @(Get-TerminalWindows)

Start-Process wt.exe -ArgumentList @(
  '-w','new','--size',"$Cols,$Rows",
  'new-tab','--suppressApplicationTitle','--title','ccdesk',
  '--', $ExePath, '--demo'
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

# Close only what we launched; exiting ccdesk makes WT close the window with it.
$demo = Get-DemoProcess -ExcludePids $beforePids
if ($demo.Count -eq 0) {
  Write-Warning "no new '$ExePath --demo' process found: closing nothing, so the capture window may still be open"
} else {
  foreach ($p in $demo) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
  "closed demo ccdesk: $(($demo | ForEach-Object { $_.ProcessId }) -join ', ')"
}

"saved: $Out size=${w}x${ht}"
