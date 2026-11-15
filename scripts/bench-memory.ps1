<#
.SYNOPSIS
  Collects the numbers for the Memory Cleaner benchmark (docs/RESEARCH-memory-cleaning.md, issue #32).

.DESCRIPTION
  The script never performs the memory action. For every run it does this:
    1. idle -IdleSeconds, then sample the memory counters ("before");
    2. pause -Wait seconds: for arms trim and purge press the matching button in Memory Cleaner
       now (arm none: do nothing);
    3. sample the counters again ("after");
    4. start -App, time how long until that process has a main window, then close it.
  One CSV row per run is appended to <OutDir>\bench-memory.csv. Needs no administrator rights,
  uses no network and writes only under -OutDir. Windows PowerShell 5.1 compatible.

  All counters are read in ONE Get-Counter call (3 one-second samples, averaged); the CSV records
  the window (sampled_at, sample_seconds). If that call fails (for example a non-English Windows),
  the counter columns are NA and Available MB / commit % come from CIM (sample_mode=cim).

  Arm "priority" is not offered: Memory Cleaner has no priority-only action (Trim idle apps lowers
  priority AND trims), so it cannot be measured separately until a build switch exists.

  Only the process this script starts is timed and closed. If the app hands over to another
  instance (the started process exits without a window) launch_ms is NA and nothing else is closed.
  Choose an -App that does not do this.

  Column action_confirmed is for you: after each run read the Last run card (or the purge result)
  in Memory Cleaner and fill it in (yes / no); runs where the action did not happen are repeated.

.EXAMPLE
  .\bench-memory.ps1 -Arm trim -Runs 5 -IdleSeconds 660 -Wait 30

.EXAMPLE
  .\bench-memory.ps1 -Arm none -DryRun
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('none', 'trim', 'purge')]
    [string]$Arm,
    [string]$App = 'notepad.exe',
    [ValidateRange(1, 100)]
    [int]$Runs = 5,
    [ValidateRange(0, 3600)]
    [int]$IdleSeconds = 30,
    # Seconds between the "before" and "after" samples: time to press the button in Memory Cleaner.
    [ValidateRange(0, 600)]
    [int]$Wait = 20,
    [string]$OutDir = (Join-Path $env:TEMP 'toolboks-bench'),
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$CsvPath = Join-Path $OutDir 'bench-memory.csv'
$SampleCount = 3          # one-second samples averaged, all counters in one call
$LaunchTimeoutMs = 30000

$Action = switch ($Arm) {
    'none'     { 'do nothing' }
    'trim'     { 'press "Trim idle apps" on the Home page (not "Trim selected"), then read the Last run card' }
    'purge'    { 'press "Clear low-priority cache" and confirm the administrator window' }
}

if ($DryRun) {
    Write-Output "DryRun: nothing is started, sampled or written."
    Write-Output "  arm=$Arm app=$App runs=$Runs idle=${IdleSeconds}s wait=${Wait}s"
    Write-Output "  csv=$CsvPath"
    Write-Output "  per run: idle ${IdleSeconds}s, sample before, ${Wait}s pause (you: $Action), sample after, launch app and time the main window, close it."
    return
}

$Counters = @{
    Available = '\Memory\Available MBytes'
    Commit    = '\Memory\% Committed Bytes In Use'
    Faults    = '\Memory\Page Faults/sec'
    PagesIn   = '\Memory\Pages Input/sec'
    StandbyN  = '\Memory\Standby Cache Normal Priority Bytes'
    StandbyR  = '\Memory\Standby Cache Reserve Bytes'
    StandbyC  = '\Memory\Standby Cache Core Bytes'
}

function Get-CounterValues {
    # One Get-Counter call for all counters (same one-second samples for every number); returns
    # path -> average over $SampleCount samples, or $null when the call fails.
    param([string[]]$Paths)
    try {
        $samples = Get-Counter -Counter $Paths -SampleInterval 1 -MaxSamples $SampleCount -ErrorAction Stop
    } catch {
        Write-Warning "Counters unavailable, using CIM fallback: $($_.Exception.Message)"
        return $null
    }
    $result = @{}
    $all = @($samples | ForEach-Object { $_.CounterSamples })
    foreach ($p in $Paths) {
        $vals = @($all | Where-Object { $_.Path.EndsWith($p.ToLowerInvariant()) } | ForEach-Object { $_.CookedValue })
        $result[$p] = if ($vals.Count -gt 0) { ($vals | Measure-Object -Average).Average } else { $null }
    }
    return $result
}

function Format-Number($v) {
    if ($null -eq $v) { return 'NA' }
    return ([double]$v).ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)
}

function Get-Snapshot {
    $startedAt = Get-Date
    $c = Get-CounterValues -Paths @($Counters.Values)
    $endedAt = Get-Date
    $mode = 'counters'
    if ($null -eq $c) {
        # Fallback when the performance counters are broken: Available MB and commit % from CIM (KB values).
        $mode = 'cim'
        $os = Get-CimInstance Win32_OperatingSystem
        $c = @{
            $Counters.Available = $os.FreePhysicalMemory / 1KB
            $Counters.Commit    = 100 * ($os.TotalVirtualMemorySize - $os.FreeVirtualMemory) / $os.TotalVirtualMemorySize
        }
    }
    $parts = @($Counters.StandbyN, $Counters.StandbyR, $Counters.StandbyC) | ForEach-Object { $c[$_] }
    $standby = $null
    if (@($parts | Where-Object { $null -eq $_ }).Count -eq 0) { $standby = (($parts | Measure-Object -Sum).Sum) / 1MB }
    return [pscustomobject]@{
        AvailableMB = $c[$Counters.Available]
        CommitPct   = $c[$Counters.Commit]
        StandbyMB   = $standby
        Faults      = $c[$Counters.Faults]
        PagesIn     = $c[$Counters.PagesIn]
        Mode        = $mode
        StartedAt   = $startedAt.ToString('o')
        Seconds     = ($endedAt - $startedAt).TotalSeconds
    }
}

function Measure-AppLaunch {
    # Milliseconds from Start-Process until the process WE started has a main window; then closes
    # only that process. If it exits without a window (handed over to another instance) the result
    # is NA and nothing else is touched. shortcut: child processes it spawned are not closed.
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $started = Start-Process -FilePath $App -PassThru
    $ms = $null
    while ($sw.ElapsedMilliseconds -lt $LaunchTimeoutMs) {
        $started.Refresh()
        if ($started.HasExited) { break }
        if ($started.MainWindowHandle -ne [IntPtr]::Zero) { $ms = $sw.ElapsedMilliseconds; break }
        Start-Sleep -Milliseconds 10
    }
    $sw.Stop()
    if ($null -eq $ms) { Write-Warning "No main window for the started process (exited or timed out after ${LaunchTimeoutMs} ms): launch_ms=NA. Pick another -App if this repeats." }
    try {
        $started.Refresh()
        if (-not $started.HasExited) {
            [void]$started.CloseMainWindow()
            if (-not $started.WaitForExit(5000)) { $started.Kill() }
        }
    } catch { Write-Warning "Could not close pid $($started.Id): $($_.Exception.Message)" }
    return $ms
}

if (-not (Get-Command $App -ErrorAction SilentlyContinue)) { throw "App not found: $App" }
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

$ramGb = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$ubr = if ($cv.PSObject.Properties['UBR']) { $cv.UBR } else { 0 }
$build = "$($cv.CurrentBuildNumber).$ubr"
Write-Output "Machine: ${ramGb} GB RAM, Windows build $build. Arm: $Arm. CSV: $CsvPath"

for ($run = 1; $run -le $Runs; $run++) {
    Write-Output "Run $run/${Runs}: idle ${IdleSeconds}s ..."
    Start-Sleep -Seconds $IdleSeconds
    $b = Get-Snapshot
    Write-Output "Run $run/${Runs}: NOW (within ${Wait}s) -> $Action"
    Start-Sleep -Seconds $Wait
    $a = Get-Snapshot
    $launch = Measure-AppLaunch
    $row = [pscustomobject][ordered]@{
        timestamp          = (Get-Date).ToString('o')
        machine_ram_gb     = Format-Number $ramGb
        windows_build      = $build
        arm                = $Arm
        run                = $run
        app                = $App
        launch_ms          = Format-Number $launch
        action_confirmed   = if ($Arm -eq 'none') { 'n/a' } else { '' }
        sample_mode        = $b.Mode + '/' + $a.Mode
        sample_seconds     = Format-Number $b.Seconds
        before_sampled_at  = $b.StartedAt
        after_sampled_at   = $a.StartedAt
        before_available_mb = Format-Number $b.AvailableMB
        after_available_mb  = Format-Number $a.AvailableMB
        before_commit_pct   = Format-Number $b.CommitPct
        after_commit_pct    = Format-Number $a.CommitPct
        before_standby_mb   = Format-Number $b.StandbyMB
        after_standby_mb    = Format-Number $a.StandbyMB
        before_faults_per_s = Format-Number $b.Faults
        after_faults_per_s  = Format-Number $a.Faults
        before_pages_in_per_s = Format-Number $b.PagesIn
        after_pages_in_per_s  = Format-Number $a.PagesIn
    }
    $row | Export-Csv -Path $CsvPath -Append -NoTypeInformation -Encoding UTF8
    Write-Output ("Run {0}: launch {1} ms, available {2} -> {3} MB" -f $run, $row.launch_ms, $row.before_available_mb, $row.after_available_mb)
}
Write-Output "Done. $Runs row(s) appended to $CsvPath"
