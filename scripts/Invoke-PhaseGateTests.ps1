param(
    [Parameter(Mandatory = $true)]
    [string]$EvidenceDirectory
)

# Runs the tests that evidence each active-phase acceptance gate. Gate tests are named after the
# gate ID (Rust: gate_<id>_*, frontend: "gate <id> ..."), so each gate is backed by its own passing
# tests rather than inheriting a generic suite result. A gate with zero matching tests fails.

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..")).TrimEnd("\")
New-Item -ItemType Directory -Path $EvidenceDirectory -Force | Out-Null
$EvidenceDirectory = [System.IO.Path]::GetFullPath($EvidenceDirectory)
$logDirectory = Join-Path $EvidenceDirectory "phase-gates"
New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null

$phaseText = Get-Content -LiteralPath (Join-Path $root "PHASE_MANIFEST.yaml") -Raw
$phase = [regex]::Match($phaseText, '(?m)^phase:\s*(RG-R\d{2})\s*$').Groups[1].Value
$gateSection = [regex]::Match($phaseText, '(?ms)^acceptance_gates:[ \t]*\r?\n(.*?)(?=^\S|\z)')
if (-not $phase -or -not $gateSection.Success) { throw "Cannot resolve the active phase acceptance gates" }
$gateIds = @([regex]::Matches($gateSection.Groups[1].Value, '(?m)^\s+- id:\s*([a-z0-9_]+)\s*$') | ForEach-Object { $_.Groups[1].Value })

# Gates evidenced by other mechanisms: the source/governance envelope and the Owner's own review.
$externallyEvidenced = @("reproducible_acceptance", "owner_ui_acceptance")
$frontendGates = @("feature_studio_ui", "desktop_strategy_ipc", "desktop_model_lab_ipc")

function Invoke-Native([scriptblock]$Command, [string]$LogPath) {
    $previous = $ErrorActionPreference
    try {
        # Native tools report progress on stderr; success is decided by the exit code.
        $ErrorActionPreference = "Continue"
        $global:LASTEXITCODE = $null
        & $Command *> $LogPath
        if ($null -eq $LASTEXITCODE) { return 1 }
        return [int]$LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Get-RustCounts([string]$LogPath) {
    $passed = 0
    $failed = 0
    foreach ($match in [regex]::Matches((Get-Content -LiteralPath $LogPath -Raw), 'test result: \w+\. (\d+) passed; (\d+) failed')) {
        $passed += [int]$match.Groups[1].Value
        $failed += [int]$match.Groups[2].Value
    }
    return @($passed, $failed)
}

function Get-VitestCounts([string]$LogPath) {
    $text = (Get-Content -LiteralPath $LogPath -Raw) -replace '\x1b\[[0-9;]*m', ''
    $line = [regex]::Match($text, '(?m)^\s*Tests\s+(.+)$').Groups[1].Value
    $passed = [regex]::Match($line, '(\d+) passed').Groups[1].Value
    $failed = [regex]::Match($line, '(\d+) failed').Groups[1].Value
    return @([int]("0$passed"), [int]("0$failed"))
}

$implementationText = Get-Content -LiteralPath (Join-Path $root "IMPLEMENTATION_STATE.yaml") -Raw
$builderStatus = [regex]::Match($implementationText, '(?m)^builder_status:\s*([A-Z_]+)\s*$').Groups[1].Value
if (-not $builderStatus) { throw "Cannot resolve the active phase builder status" }
if ($builderStatus -eq "NOT_STARTED") {
    # An authorized phase with no implementation has no gate tests yet. Every gate stays NOT_RUN (no
    # RG_GATE_* outcome is exported), so the evidence report cannot show a pass; nothing is executed.
    $summary = [ordered]@{
        schema_version = 1
        phase = $phase
        generated_utc = [DateTime]::UtcNow.ToString("o")
        builder_status = $builderStatus
        gates = @()
    }
    $summaryPath = Join-Path $EvidenceDirectory "phase-gates.json"
    [System.IO.File]::WriteAllText($summaryPath, ($summary | ConvertTo-Json -Depth 8) + "`n", [System.Text.UTF8Encoding]::new($false))
    Write-Host "PHASE_GATE_SUMMARY=$summaryPath"
    Write-Host "PHASE_GATE_TESTS=NOT_RUN (builder_status NOT_STARTED for $phase)"
    exit 0
}

$results = [System.Collections.Generic.List[object]]::new()
foreach ($gateId in $gateIds) {
    if ($gateId -in $externallyEvidenced) { continue }
    $rustLog = Join-Path $logDirectory "$gateId.rust.log"
    $rustExit = Invoke-Native { cargo test --workspace --locked -- "gate_$gateId" } $rustLog
    $rustCounts = Get-RustCounts $rustLog
    $frontendExit = 0
    $frontendCounts = @(0, 0)
    $frontendLog = $null
    if ($gateId -in $frontendGates) {
        $frontendLog = Join-Path $logDirectory "$gateId.frontend.log"
        Push-Location -LiteralPath $root
        try {
            $frontendExit = Invoke-Native { npx --no-install vitest --run -t "gate $gateId" } $frontendLog
        } finally {
            Pop-Location
        }
        $frontendCounts = Get-VitestCounts $frontendLog
    }
    $passed = $rustCounts[0] + $frontendCounts[0]
    $failed = $rustCounts[1] + $frontendCounts[1]
    $frontendCovered = ($gateId -notin $frontendGates) -or $frontendCounts[0] -gt 0
    $success = $rustExit -eq 0 -and $frontendExit -eq 0 -and $failed -eq 0 -and $rustCounts[0] -gt 0 -and $frontendCovered
    $outcome = if ($success) { "success" } else { "failure" }
    [Environment]::SetEnvironmentVariable("RG_GATE_$gateId", $outcome)
    if ($env:GITHUB_ENV) {
        [System.IO.File]::AppendAllLines($env:GITHUB_ENV, [string[]]@("RG_GATE_$gateId=$outcome"), [System.Text.UTF8Encoding]::new($false))
    }
    $results.Add([pscustomobject]@{
        id = $gateId
        outcome = $outcome
        rust_tests_passed = $rustCounts[0]
        rust_tests_failed = $rustCounts[1]
        frontend_tests_passed = $frontendCounts[0]
        frontend_tests_failed = $frontendCounts[1]
        rust_log = [System.IO.Path]::GetFileName($rustLog)
        frontend_log = if ($frontendLog) { [System.IO.Path]::GetFileName($frontendLog) } else { $null }
    })
    Write-Host ("GATE {0} {1} rust={2}/{3} frontend={4}/{5}" -f $gateId, $outcome.ToUpperInvariant(), $rustCounts[0], ($rustCounts[0] + $rustCounts[1]), $frontendCounts[0], ($frontendCounts[0] + $frontendCounts[1]))
}

$summary = [ordered]@{
    schema_version = 1
    phase = $phase
    generated_utc = [DateTime]::UtcNow.ToString("o")
    gates = @($results)
}
$summaryPath = Join-Path $EvidenceDirectory "phase-gates.json"
[System.IO.File]::WriteAllText($summaryPath, ($summary | ConvertTo-Json -Depth 8) + "`n", [System.Text.UTF8Encoding]::new($false))
$failures = @($results | Where-Object outcome -ne "success")
Write-Host "PHASE_GATE_SUMMARY=$summaryPath"
if ($results.Count -eq 0 -or $failures.Count -gt 0) {
    Write-Host "PHASE_GATE_TESTS=FAIL"
    exit 1
}
Write-Host "PHASE_GATE_TESTS=PASS"
exit 0
