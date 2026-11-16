<#
.SYNOPSIS
  Unattended version of the Memory Cleaner benchmark (issue #32): no window, no button press, and
  nothing is touched that the benchmark did not start.

.DESCRIPTION
  scripts\bench-memory.ps1 needs a person to press "Trim idle apps" at the NOW prompt. This
  driver presses it for them, without a window:
    1. It starts a few benchmark-owned child processes (the memory-cleaner test exe in
       `bench::bench_child_workload` mode) that each hold -MB megabytes and stay idle.
    2. For every arm in -Order it runs bench-memory.ps1 -Runs 1 -Headless. At the NOW prompt, for
       arm trim it runs `bench::bench_trim_action`: the add-on's own manual plan and runner
       (lower memory priority, then trim) over those children only. Arm none does nothing.
    3. After the run it asks the children to re-read all of their memory and records how long that
       took (retouch_ms): the cost of the next use. Then the next run starts.
  The purge arm is not offered: it needs the administrator window (a UAC prompt), which stays
  manual. No other process is read, lowered, trimmed or closed.

  The test exe is built with: cargo test -p memory-cleaner --lib --no-run  (test code only, never
  part of the add-on exe). Pass its path as -TestExe.

  Output: bench-memory.csv (the usual rows; run and action_confirmed filled in) and
  bench-driven.csv (one row per run: retouch times, the trim numbers) under -OutDir.

.EXAMPLE
  .\bench-memory-driven.ps1 -TestExe D:\target\debug\deps\memory_cleaner-1234.exe -Order none,trim,trim,none
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string]$TestExe,
    [Parameter(Mandatory = $true)] [string[]]$Order,
    [ValidateRange(1, 8)] [int]$Children = 3,
    [ValidateRange(120, 1000)] [int]$MB = 300,
    # Must be at least 60 (the manual policy needs an app idle for a minute).
    [ValidateRange(60, 3600)] [int]$IdleSeconds = 90,
    [ValidateRange(5, 600)] [int]$Wait = 15,
    [string]$OutDir = (Join-Path $env:TEMP 'toolboks-bench')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Order = @($Order | ForEach-Object { $_ -split ',' } | Where-Object { $_ })   # -File passes "a,b" as one string
foreach ($a in $Order) { if ($a -notin 'none', 'trim') { throw "Arm '$a' is not driven here (only none and trim; purge needs the administrator window)." } }
if (-not (Test-Path $TestExe)) { throw "Test exe not found: $TestExe" }

function Read-Shared([string]$Path) {
    # The child writes this file while we read it: open it with sharing.
    try {
        $fs = [IO.File]::Open($Path, 'Open', 'Read', 'ReadWrite')
        try { return (New-Object IO.StreamReader($fs)).ReadToEnd() } finally { $fs.Dispose() }
    } catch { return '' }
}

$bench = Join-Path $PSScriptRoot 'bench-memory.ps1'
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$work = Join-Path $OutDir 'work'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Path $work | Out-Null
$csv = Join-Path $OutDir 'bench-memory.csv'
$extraCsv = Join-Path $OutDir 'bench-driven.csv'
$before = if (Test-Path $csv) { @(Import-Csv $csv).Count } else { 0 }

$env:TB_BENCH_DIR = $work
$env:TB_BENCH_MB = "$MB"
$childArgs = @('--ignored', '--exact', 'bench::bench_child_workload', '--nocapture', '--test-threads=1')
$kids = @()
for ($i = 0; $i -lt $Children; $i++) {
    $kids += Start-Process -FilePath $TestExe -ArgumentList $childArgs -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $work "child$i.out") -RedirectStandardError (Join-Path $work "child$i.err")
}
$deadline = (Get-Date).AddSeconds(120)
while (@(Get-ChildItem $work -Filter 'child-*.ready').Count -lt $Children) {
    if ((Get-Date) -gt $deadline) { throw 'Benchmark children did not start.' }
    Start-Sleep -Milliseconds 200
}
Write-Output "Children ready: $($kids.Id -join ', ') ($MB MB each). Order: $($Order -join ',')."
$lastTouch = Get-Date
$seq = 0
$extras = @()
$failed = $false

try {
    for ($i = 0; $i -lt $Order.Count; $i++) {
        $arm = $Order[$i]
        $out = Join-Path $work "run$i.out"
        $argLine = "-NoProfile -ExecutionPolicy Bypass -File `"$bench`" -Arm $arm -Runs 1 -IdleSeconds $IdleSeconds -Wait $Wait -Headless -App powershell.exe -OutDir `"$OutDir`""
        $p = Start-Process -FilePath 'powershell.exe' -ArgumentList $argLine -WindowStyle Hidden -PassThru -RedirectStandardOutput $out
        $trim = $null
        while (-not $p.HasExited) {
            Start-Sleep -Milliseconds 250
            if ($arm -eq 'trim' -and $null -eq $trim) {
                $text = Read-Shared $out
                if ($text -match 'NOW') {
                    $env:TB_BENCH_IDLE_SECS = [string][int]((Get-Date) - $lastTouch).TotalSeconds
                    $keep = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
                    $lines = & $TestExe --ignored --exact bench::bench_trim_action --nocapture --test-threads=1 2>&1
                    $ErrorActionPreference = $keep
                    $found = @($lines | Where-Object { "$_" -match 'BENCH-TRIM ' })
                    $trim = if ($found.Count -gt 0) { "$($found[0])".Substring("$($found[0])".IndexOf('BENCH-TRIM ')) } else { $null }
                    if (-not $trim) { $trim = 'BENCH-TRIM missing: ' + ($lines -join ' | ') }
                    Write-Output "Run $($i + 1) ($arm): $trim"
                }
            }
        }
        $p.WaitForExit()
        # (ExitCode is unreliable for a redirected Start-Process; the new CSV row is the proof.)
        if (@(Import-Csv $csv).Count -ne $before + $i + 1) { throw "bench-memory.ps1 wrote no row for run $($i + 1) ($arm): $(Read-Shared $out)" }

        # Cost of the next use: every child re-reads all of its memory, timed inside the child.
        $seq++
        Set-Content -Path (Join-Path $work 'touch.req') -Value $seq
        $deadline = (Get-Date).AddSeconds(120)
        while (@(Get-ChildItem $work -Filter "touched-*.$seq").Count -lt $Children) {
            if ((Get-Date) -gt $deadline) { throw 'A child did not answer the re-read request.' }
            Start-Sleep -Milliseconds 100
        }
        $ms = @(Get-ChildItem $work -Filter "touched-*.$seq" | ForEach-Object { [double]((Get-Content $_.FullName -Raw).Split(' ')[0]) })
        $lastTouch = Get-Date

        $f = @{}
        if ($trim) { foreach ($m in [regex]::Matches($trim, '(\w+)=(\d+)')) { $f[$m.Groups[1].Value] = [int]$m.Groups[2].Value } }
        $confirmed = if ($arm -eq 'none') { 'n/a' } elseif ($f.ContainsKey('trimmed') -and $f['trimmed'] -ge 1 -and $f['failed'] -eq 0) { 'yes' } else { 'no' }
        $extras += [pscustomobject][ordered]@{
            order_index      = $i + 1
            arm              = $arm
            action_confirmed = $confirmed
            retouch_ms_sum   = ($ms | Measure-Object -Sum).Sum.ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)
            retouch_ms_max   = ($ms | Measure-Object -Maximum).Maximum.ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)
            children         = $Children
            mb_each          = $MB
            idle_seconds     = $IdleSeconds
            trimmed          = if ($f.ContainsKey('trimmed')) { $f['trimmed'] } else { 'NA' }
            lowered          = if ($f.ContainsKey('lowered')) { $f['lowered'] } else { 'NA' }
            failed           = if ($f.ContainsKey('failed')) { $f['failed'] } else { 'NA' }
            ws_before_mb     = if ($f.ContainsKey('ws_before_mb')) { $f['ws_before_mb'] } else { 'NA' }
            ws_after_mb      = if ($f.ContainsKey('ws_after_mb')) { $f['ws_after_mb'] } else { 'NA' }
        }
        Write-Output ("Run {0} ({1}): re-read {2} ms total (max {3}), action_confirmed={4}" -f ($i + 1), $arm, $extras[-1].retouch_ms_sum, $extras[-1].retouch_ms_max, $confirmed)
    }
}
catch { Write-Output ("ERROR: " + $_.Exception.Message + "`n" + $_.ScriptStackTrace); $failed = $true }
finally {
    Set-Content -Path (Join-Path $work 'stop') -Value 'stop'
    foreach ($k in $kids) { if (-not $k.WaitForExit(10000)) { try { $k.Kill() } catch { } } }
}

if ($failed) { exit 1 }

# Fill run (per arm) and action_confirmed in the CSV rows this session appended.
$rows = @(Import-Csv $csv)
$counts = @{}
for ($i = 0; $i -lt $Order.Count; $i++) {
    $row = $rows[$before + $i]
    if ($row.arm -ne $Order[$i]) { throw "CSV row $($before + $i + 1) is arm $($row.arm), expected $($Order[$i])." }
    $counts[$row.arm] = 1 + [int]$(if ($counts.ContainsKey($row.arm)) { $counts[$row.arm] } else { 0 })
    $row.run = $counts[$row.arm]
    $row.action_confirmed = $extras[$i].action_confirmed
}
$rows | Export-Csv -Path $csv -NoTypeInformation -Encoding UTF8
$extras | Export-Csv -Path $extraCsv -Append -NoTypeInformation -Encoding UTF8
Remove-Item -Recurse -Force $work
Write-Output "Done. $($Order.Count) run(s): $csv and $extraCsv"
