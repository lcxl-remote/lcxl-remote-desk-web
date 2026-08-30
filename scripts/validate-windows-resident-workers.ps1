[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$LogPath,
    [int]$DurationSeconds = 300,
    [int]$ExpectedTransitions = 40,
    [int]$SampleIntervalMs = 200,
    [string]$ServerExecutableName = "lcxl-remote-desk-server.exe",
    [string]$EvidenceDirectory = ".\resident-worker-evidence"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-ResidentWorkerSnapshot {
    param([string]$ExecutableName)

    @(Get-CimInstance Win32_Process | Where-Object {
        $_.Name -eq $ExecutableName -and
        $_.CommandLine -match "--startup-mode\s+session-worker"
    } | ForEach-Object {
        [pscustomobject]@{
            CapturedAtUtc = [DateTime]::UtcNow.ToString("o")
            ProcessId = [uint32]$_.ProcessId
            ParentProcessId = [uint32]$_.ParentProcessId
            SessionId = [uint32]$_.SessionId
            CommandLine = [string]$_.CommandLine
        }
    })
}

function Get-Percentile {
    param([long[]]$Values, [double]$Percentile)

    if ($Values.Count -eq 0) {
        return $null
    }
    $sorted = @($Values | Sort-Object)
    $index = [Math]::Max(0, [Math]::Ceiling($Percentile * $sorted.Count) - 1)
    return [long]$sorted[$index]
}

function Read-NewLogText {
    param([string]$Path, [long]$InitialLength)

    $item = Get-Item -LiteralPath $Path
    $offset = if ($item.Length -ge $InitialLength) { $InitialLength } else { 0 }
    $stream = [System.IO.File]::Open(
        $item.FullName,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
    )
    try {
        [void]$stream.Seek($offset, [System.IO.SeekOrigin]::Begin)
        $reader = [System.IO.StreamReader]::new($stream)
        try {
            return $reader.ReadToEnd()
        }
        finally {
            $reader.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}

$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this validation from an elevated PowerShell session."
}
if (-not (Test-Path -LiteralPath $LogPath -PathType Leaf)) {
    throw "Log file does not exist: $LogPath"
}
if ($DurationSeconds -lt 1 -or $ExpectedTransitions -lt 1 -or $SampleIntervalMs -lt 50) {
    throw "DurationSeconds/ExpectedTransitions/SampleIntervalMs are outside their valid ranges."
}

$evidencePath = [System.IO.Path]::GetFullPath($EvidenceDirectory)
[void](New-Item -ItemType Directory -Path $evidencePath -Force)
$startedAt = [DateTime]::UtcNow
$initialLogLength = (Get-Item -LiteralPath $LogPath).Length
$baseline = Get-ResidentWorkerSnapshot -ExecutableName $ServerExecutableName
if ($baseline.Count -lt 2) {
    throw "Expected at least one Default/Winlogon worker pair, found $($baseline.Count). Is LRD_EXPERIMENTAL_WINDOWS_RESIDENT_WORKERS=1 enabled on the service?"
}

$baselineGroups = @($baseline | Group-Object SessionId)
$invalidGroups = @($baselineGroups | Where-Object { $_.Count -ne 2 })
if ($invalidGroups.Count -gt 0) {
    $description = ($invalidGroups | ForEach-Object { "session=$($_.Name),workers=$($_.Count)" }) -join "; "
    throw "Each resident WTS session must have exactly two workers at baseline: $description"
}

$baseline | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $evidencePath "baseline-workers.json") -Encoding utf8
Get-ComputerInfo | Select-Object WindowsProductName, WindowsVersion, OsBuildNumber, OsArchitecture |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidencePath "host.json") -Encoding utf8

Write-Host "Monitoring resident workers for $DurationSeconds seconds."
Write-Host "Keep one remote desktop PC connected and perform $ExpectedTransitions desktop transitions (normally 20 UAC enter/return cycles)."
Write-Host "Also verify manually that UAC accepts remote keyboard input and that terminal/file/AI commands still run as the session user."

$samples = [System.Collections.Generic.List[object]]::new()
$deadline = [DateTime]::UtcNow.AddSeconds($DurationSeconds)
while ([DateTime]::UtcNow -lt $deadline) {
    foreach ($sample in (Get-ResidentWorkerSnapshot -ExecutableName $ServerExecutableName)) {
        $samples.Add($sample)
    }
    Start-Sleep -Milliseconds $SampleIntervalMs
}

$finishedAt = [DateTime]::UtcNow
$final = Get-ResidentWorkerSnapshot -ExecutableName $ServerExecutableName
$samples | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $evidencePath "worker-samples.json") -Encoding utf8
$final | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $evidencePath "final-workers.json") -Encoding utf8

$newLogText = Read-NewLogText -Path $LogPath -InitialLength $initialLogLength
$newLogText | Set-Content -LiteralPath (Join-Path $evidencePath "validation-log.txt") -Encoding utf8
$lines = @($newLogText -split "`r?`n")
$stagePattern = [regex]'resident_switch stage=(deactivate_applied|activate_applied|media_replayed|first_idr).*?route_epoch=([^\s]+).*?elapsed_ms=(\d+)'
$events = [System.Collections.Generic.List[object]]::new()
foreach ($line in $lines) {
    $match = $stagePattern.Match($line)
    if ($match.Success) {
        $events.Add([pscustomobject]@{
            Stage = $match.Groups[1].Value
            RouteEpoch = $match.Groups[2].Value
            ElapsedMs = [long]$match.Groups[3].Value
            Line = $line
        })
    }
}

$baselinePids = @($baseline | ForEach-Object ProcessId | Sort-Object -Unique)
$observedPidValues = @($samples | ForEach-Object ProcessId) + @($final | ForEach-Object ProcessId)
$observedPids = @($observedPidValues | Sort-Object -Unique)
$finalPids = @($final | ForEach-Object ProcessId | Sort-Object -Unique)
$newPids = @($observedPids | Where-Object { $_ -notin $baselinePids })
$missingPids = @($baselinePids | Where-Object { $_ -notin $finalPids })
$firstIdr = @($events | Where-Object Stage -eq "first_idr")
$deactivate = @($events | Where-Object Stage -eq "deactivate_applied")
$activate = @($events | Where-Object Stage -eq "activate_applied")
$replayed = @($events | Where-Object Stage -eq "media_replayed")
$firstIdrLatencies = [long[]]@($firstIdr.ElapsedMs)
$p95 = Get-Percentile -Values $firstIdrLatencies -Percentile 0.95
$p99 = Get-Percentile -Values $firstIdrLatencies -Percentile 0.99

$failures = [System.Collections.Generic.List[string]]::new()
if ($newPids.Count -gt 0 -or $missingPids.Count -gt 0) {
    $failures.Add("Worker PID set changed during desktop transitions (new=$($newPids -join ','), missing=$($missingPids -join ',')).")
}
foreach ($entry in @(
    [pscustomobject]@{ Name = "deactivate_applied"; ValueCount = $deactivate.Count },
    [pscustomobject]@{ Name = "activate_applied"; ValueCount = $activate.Count },
    [pscustomobject]@{ Name = "media_replayed"; ValueCount = $replayed.Count },
    [pscustomobject]@{ Name = "first_idr"; ValueCount = $firstIdr.Count }
)) {
    if ($entry.ValueCount -lt $ExpectedTransitions) {
        $failures.Add("Expected at least $ExpectedTransitions $($entry.Name) events, found $($entry.ValueCount).")
    }
}
if ($null -ne $p95 -and $p95 -gt 500) {
    $failures.Add("First-IDR P95 is ${p95}ms, above 500ms.")
}
if ($null -ne $p99 -and $p99 -gt 1000) {
    $failures.Add("First-IDR P99 is ${p99}ms, above 1000ms.")
}
if ($newLogText -match "interactive route acknowledgement timed out") {
    $failures.Add("At least one interactive-route acknowledgement timed out.")
}
if ($newLogText -match "refusing desktop switch") {
    $failures.Add("At least one desktop switch was refused.")
}

$report = [ordered]@{
    StartedAtUtc = $startedAt.ToString("o")
    FinishedAtUtc = $finishedAt.ToString("o")
    BaselineWorkerPids = $baselinePids
    FinalWorkerPids = $finalPids
    EventCounts = [ordered]@{
        DeactivateApplied = $deactivate.Count
        ActivateApplied = $activate.Count
        MediaReplayed = $replayed.Count
        FirstIdr = $firstIdr.Count
    }
    FirstIdrLatencyMs = [ordered]@{
        P95 = $p95
        P99 = $p99
        Samples = $firstIdrLatencies
    }
    AutomatedFailures = @($failures)
    ManualGatesStillRequired = @(
        "Remote keyboard input reaches the UAC prompt only after activation.",
        "AI Computer Use and password observation remain unavailable on Winlogon.",
        "Terminal/file/AI commands run as the selected session user, never SYSTEM.",
        "No WebRTC renegotiation occurs during desktop transitions."
    )
}
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $evidencePath "report.json") -Encoding utf8

if ($failures.Count -gt 0) {
    Write-Error ("Windows resident-worker validation failed:`n- " + ($failures -join "`n- "))
    exit 1
}

Write-Host "Automated resident-worker checks passed. Review report.json and complete the listed manual gates before setting WINDOWS_RESIDENT_WORKER_UAC_GO."
