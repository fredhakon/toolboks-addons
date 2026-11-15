<#
.SYNOPSIS
  Collects the numbers for the Memory Cleaner benchmark (docs/RESEARCH-memory-cleaning.md, issue #32).

.DESCRIPTION
  The script never performs the memory action. For every run it does this:
    1. idle -IdleSeconds, then sample the memory counters ("before");
    2. pause -Wait seconds: for arms priority, trim and purge press the matching button in
       Memory Cleaner now (arm none: do nothing);
    3. sample the counters again ("after");
    4. start -App, time how long until it has a main window, then close it.
  One CSV row per run is appended to <OutDir>\bench-memory.csv. Needs no administrator rights,
  uses no network and writes only under -OutDir. Windows PowerShell 5.1 compatible.

  Counter names are English; on a non-English Windows a failed counter is written as NA.

.EXAMPLE
  .\bench-memory.ps1 -Arm trim -Runs 5 -IdleSeconds 300 -Wait 30

.EXAMPLE
  .\bench-memory.ps1 -Arm none -DryRun
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('none', 'priority', 'trim', 'purge')]
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
$SampleCount = 3          # one-second samples averaged for each rate counter
$LaunchTimeoutMs = 30000

$Action = switch ($Arm) {
    'none'     { 'do nothing' }
    'priority' { 'press the memory priority action (Optimize / lower priority only, no trim)' }
    'trim'     { 'press the targeted trim (Trim selected / Optimize with trim)' }
    'purge'    { 'press "Clear low-priority cache" and confirm the administrator window' }
}

if ($DryRun) {
    Write-Output "DryRun: nothing is started, sampled or written."
    Write-Output "  arm=$Arm app=$App runs=$Runs idle=${IdleSeconds}s wait=${Wait}s"
    Write-Output "  csv=$CsvPath"
    Write-Output "  per run: idle ${IdleSeconds}s, sample before, ${Wait}s pause (you: $Action), sample after, launch app and time the main window, close it."
    return
}

function Get-CounterValues {
    # Averages each path over $SampleCount one-second samples; NA when a counter is unavailable.
    # One counter at a time, so a single broken or localized counter does not blank the others.
    param([string[]]$Paths)
    $result = @{}
    foreach ($p in $Paths) {
        $result[$p] = $null
        try {
            $samples = Get-Counter -Counter $p -SampleInterval 1 -MaxSamples $SampleCount -ErrorAction Stop
            $result[$p] = ($samples | ForEach-Object { $_.CounterSamples[0].CookedValue } | Measure-Object -Average).Average
        } catch {
            if (-not $script:WarnedCounters.ContainsKey($p)) {
                $script:WarnedCounters[$p] = $true
                Write-Warning "Counter $p unavailable: $($_.Exception.Message)"
            }
        }
    }
    return $result
}

$script:WarnedCounters = @{}
$Counters = @{
    Available = '\Memory\Available MBytes'
    Commit    = '\Memory\% Committed Bytes In Use'
    Faults    = '\Memory\Page Faults/sec'
    PagesIn   = '\Memory\Pages Input/sec'
    StandbyN  = '\Memory\Standby Cache Normal Priority Bytes'
    StandbyR  = '\Memory\Standby Cache Reserve Bytes'
    StandbyC  = '\Memory\Standby Cache Core Bytes'
}

function Format-Number($v) {
    if ($null -eq $v) { return 'NA' }
    return ([double]$v).ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)
}

function Get-Snapshot {
    $c = Get-CounterValues -Paths @($Counters.Values)
    $standby = $null
    $parts = @($c[$Counters.StandbyN], $c[$Counters.StandbyR], $c[$Counters.StandbyC]) | Where-Object { $null -ne $_ }
    if (@($parts).Count -gt 0) { $standby = (($parts | Measure-Object -Sum).Sum) / 1MB }
    $availableMb = $c[$Counters.Available]
    $commitPct = $c[$Counters.Commit]
    if ($null -eq $availableMb -or $null -eq $commitPct) {
        # Fallback when the performance counters are broken: same two numbers from CIM (KB values).
        $os = Get-CimInstance Win32_OperatingSystem
        if ($null -eq $availableMb) { $availableMb = $os.FreePhysicalMemory / 1KB }
        if ($null -eq $commitPct) { $commitPct = 100 * ($os.TotalVirtualMemorySize - $os.FreeVirtualMemory) / $os.TotalVirtualMemorySize }
    }
    return [pscustomobject]@{
        AvailableMB = $availableMb
        CommitPct   = $commitPct
        StandbyMB   = $standby
        Faults      = $c[$Counters.Faults]
        PagesIn     = $c[$Counters.PagesIn]
    }
}

function Measure-AppLaunch {
    # Milliseconds from Start-Process until a new process of the app has a main window; then closes it.
    # shortcut: matches by image name, so an app that hands over to an already running instance reports NA.
    $name = [IO.Path]::GetFileNameWithoutExtension($App)
    $before = @(Get-Process -Name $name -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $started = Start-Process -FilePath $App -PassThru
    $ms = $null
    $mine = @()
    while ($sw.ElapsedMilliseconds -lt $LaunchTimeoutMs) {
        $mine = @(Get-Process -Name $name -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Id })
        if ($mine | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero }) { $ms = $sw.ElapsedMilliseconds; break }
        Start-Sleep -Milliseconds 10
    }
    $sw.Stop()
    if ($null -eq $ms) { Write-Warning "No main window within ${LaunchTimeoutMs} ms." }
    foreach ($p in $mine + @($started)) {
        try {
            $p.Refresh()
            if (-not $p.HasExited) {
                [void]$p.CloseMainWindow()
                if (-not $p.WaitForExit(5000)) { $p.Kill() }
            }
        } catch { Write-Warning "Could not close pid $($p.Id): $($_.Exception.Message)" }
    }
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
