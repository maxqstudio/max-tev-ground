param(
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

function Get-StepOutcome([string]$Name) {
    $value = [Environment]::GetEnvironmentVariable("RG_STEP_$Name")
    if ([string]::IsNullOrWhiteSpace($value)) { return "NOT_RUN" }
    return $value.Trim().ToUpperInvariant()
}

function Get-GateStatus([string[]]$Outcomes) {
    if ($Outcomes -contains "FAILURE" -or $Outcomes -contains "CANCELLED") { return "FAIL" }
    if ($Outcomes.Count -eq 0 -or $Outcomes -contains "NOT_RUN" -or $Outcomes -contains "SKIPPED") {
        return "NOT_RUN"
    }
    if (@($Outcomes | Where-Object { $_ -ne "SUCCESS" }).Count -eq 0) { return "PASS" }
    return "NOT_RUN"
}

function New-StepEvidence([string]$Name, [string]$Outcome) {
    return [pscustomobject]@{ name = $Name; outcome = $Outcome }
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$workspace = [Environment]::GetEnvironmentVariable("GITHUB_WORKSPACE")
if ([string]::IsNullOrWhiteSpace($workspace)) { $workspace = (Get-Location).Path }

$candidateSha = "UNAVAILABLE"
$treeSha = "UNAVAILABLE"
$dirtyEntries = @()
try {
    $candidateSha = (& git -C $workspace rev-parse HEAD 2>$null).Trim()
    if ($LASTEXITCODE -ne 0) { throw "git rev-parse HEAD failed" }
    $treeSha = (& git -C $workspace rev-parse 'HEAD^{tree}' 2>$null).Trim()
    if ($LASTEXITCODE -ne 0) { throw "git rev-parse HEAD^{tree} failed" }
    $dirtyEntries = @(& git -C $workspace status --porcelain --untracked-files=normal 2>$null)
    if ($LASTEXITCODE -ne 0) { throw "git status failed" }
} catch {
    $dirtyEntries = @("SOURCE_INSPECTION_FAILED: $($_.Exception.Message)")
}

$expectedSha = [Environment]::GetEnvironmentVariable("GITHUB_SHA")
$sourceMatchesEvent = $candidateSha -eq $expectedSha -and $candidateSha -match '^[0-9a-f]{40,64}$'
$sourceIsClean = $dirtyEntries.Count -eq 0
$exactSourceStatus = if ($sourceMatchesEvent -and $sourceIsClean) { "PASS" } else { "FAIL" }

$stepNames = @(
    "source_check", "evidence_contract_tests", "rust_fmt", "roadmap_governance", "reproducible_acceptance", "rust_tests", "rust_clippy", "rust_build",
    "npm_ci", "frontend_lint", "frontend_typecheck", "frontend_tests", "frontend_build", "msvc_import", "tauri_build"
)
$steps = [ordered]@{}
foreach ($name in $stepNames) { $steps[$name] = Get-StepOutcome $name }

$rustSteps = @("rust_fmt", "rust_tests", "rust_clippy", "rust_build")
$frontendSteps = @("npm_ci", "frontend_lint", "frontend_typecheck", "frontend_tests", "frontend_build")
$rustStatus = Get-GateStatus @($rustSteps | ForEach-Object { $steps[$_] })
$evidenceContractStatus = Get-GateStatus @($steps["evidence_contract_tests"])
$frontendStatus = Get-GateStatus @($frontendSteps | ForEach-Object { $steps[$_] })
$roadmapStatus = Get-GateStatus @($steps["roadmap_governance"])
$tauriStatus = Get-GateStatus @($steps["tauri_build"])
if ($steps["msvc_import"] -in @("FAILURE", "CANCELLED") -and $steps["tauri_build"] -in @("NOT_RUN", "SKIPPED")) {
    $tauriStatus = "ENVIRONMENT_BLOCKED"
}
$implementationPath = Join-Path $workspace "IMPLEMENTATION_STATE.yaml"
$phaseManifestPath = Join-Path $workspace "PHASE_MANIFEST.yaml"
$implementationSource = Get-Content -LiteralPath $implementationPath -Raw
$phaseSource = Get-Content -LiteralPath $phaseManifestPath -Raw
$phaseMatch = [regex]::Match($implementationSource, '(?m)^current_phase:\s*(RG-R\d{2})\s*$')
$uiMatch = [regex]::Match($implementationSource, '(?m)^owner_ui_status:\s*([A-Z_]+)\s*$')
$gateSection = [regex]::Match($phaseSource, '(?ms)^acceptance_gates:\s*(.*?)(?=^\S|\z)')
if (-not $phaseMatch.Success -or -not $uiMatch.Success -or -not $gateSection.Success) {
    throw "Cannot resolve active phase, Owner UI status, or mandatory acceptance gates"
}
$phaseName = $phaseMatch.Groups[1].Value
$ownerUiStatus = $uiMatch.Groups[1].Value
$ownerUiReportStatus = switch ($ownerUiStatus) {
    "OWNER_ACCEPTED" { "OWNER_ACCEPTED" }
    "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED" { "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED" }
    default { "OWNER_UI_ACCEPTANCE_PENDING" }
}
$gateIds = @([regex]::Matches($gateSection.Groups[1].Value, '(?m)^\s+- id:\s*([a-z0-9_]+)\s*$') | ForEach-Object { $_.Groups[1].Value })
if ($gateIds.Count -eq 0 -or @($gateIds | Select-Object -Unique).Count -ne $gateIds.Count) {
    throw "The active phase acceptance gate IDs are empty or duplicated"
}
$steps["reproducible_acceptance"] = [Environment]::GetEnvironmentVariable("RG_STEP_reproducible_acceptance")
if ([string]::IsNullOrWhiteSpace($steps["reproducible_acceptance"])) { $steps["reproducible_acceptance"] = "NOT_RUN" }
$gates = @(
    foreach ($id in $gateIds) {
        if ($id -eq "owner_ui_acceptance") {
            $gateStatus = switch ($ownerUiReportStatus) {
                "OWNER_ACCEPTED" { "PASS" }
                "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED" { "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED" }
                default { "OWNER_UI_ACCEPTANCE_PENDING" }
            }
            $gateEvidence = "implementation_state.owner_ui_status=$ownerUiStatus"
        } else {
            $outcome = [Environment]::GetEnvironmentVariable("RG_GATE_$id")
            if ([string]::IsNullOrWhiteSpace($outcome)) { $outcome = "NOT_RUN" }
            $outcome = $outcome.Trim().ToUpperInvariant()
            $gateStatus = switch ($outcome) {
                "SUCCESS" { "PASS" }
                "PASS" { "PASS" }
                "FAILURE" { "FAIL" }
                "CANCELLED" { "FAIL" }
                "FAIL" { "FAIL" }
                "ENVIRONMENT_BLOCKED" { "ENVIRONMENT_BLOCKED" }
                default { "NOT_RUN" }
            }
            $gateEvidence = "RG_GATE_$id=$outcome; source_sha=$candidateSha; tree_sha=$treeSha"
        }
        [pscustomobject]@{ id = $id; status = $gateStatus; evidence = $gateEvidence }
    }
)

$automatedGateStatuses = @($gates | Where-Object id -ne "owner_ui_acceptance" | ForEach-Object status)
$automatedGates = @($rustStatus, $frontendStatus, $roadmapStatus, $tauriStatus, $evidenceContractStatus) + $automatedGateStatuses
if ($exactSourceStatus -eq "FAIL" -or $automatedGates -contains "FAIL") {
    $automatedStatus = "FAILED"
} elseif ($automatedGates -contains "ENVIRONMENT_BLOCKED") {
    $automatedStatus = "ENVIRONMENT_BLOCKED"
} elseif ($automatedGates -contains "NOT_RUN") {
    $automatedStatus = "NOT_RUN"
} else {
    $automatedStatus = "AUTOMATED_PASS"
}

function Get-ToolVersion([string]$Command, [string[]]$Arguments) {
    try {
        $commandInfo = Get-Command $Command -ErrorAction Stop
        $versionOutput = & $commandInfo.Source @Arguments 2>$null | Select-Object -First 1
        if ($LASTEXITCODE -eq 0 -and $versionOutput) { return [string]$versionOutput }
    } catch { }
    return "UNAVAILABLE"
}

$runUrl = "https://github.com/$env:GITHUB_REPOSITORY/actions/runs/$env:GITHUB_RUN_ID"
$report = [ordered]@{
    schema_version = 1
    phase = $phaseName
    generated_utc = [DateTime]::UtcNow.ToString("o")
    automated_status = $automatedStatus
    next_phase_unlocked = $false
    source = [ordered]@{
        repository = $env:GITHUB_REPOSITORY
        ref = $env:GITHUB_REF
        commit_sha = $candidateSha
        event_sha = $expectedSha
        tree_sha = $treeSha
        clean = $sourceIsClean
        dirty_entry_count = $dirtyEntries.Count
        exact_event_match = $sourceMatchesEvent
    }
    workflow = [ordered]@{
        name = $env:GITHUB_WORKFLOW
        run_id = $env:GITHUB_RUN_ID
        run_attempt = $env:GITHUB_RUN_ATTEMPT
        event_name = $env:GITHUB_EVENT_NAME
        run_url = $runUrl
        validation_step_outcomes = $steps
    }
    environment = [ordered]@{
        os = [Environment]::OSVersion.VersionString
        rustc = Get-ToolVersion "rustc" @("--version")
        node = Get-ToolVersion "node" @("--version")
        npm = Get-ToolVersion "npm" @("--version")
    }
    owner_ui_status = $ownerUiReportStatus
    legacy_baseline_status = "NOT_VERIFIED_BY_GITHUB_CI"
    gates = $gates
}

$reportPath = Join-Path $OutputDirectory "acceptance.json"
$json = $report | ConvertTo-Json -Depth 16
[System.IO.File]::WriteAllText($reportPath, $json + "`n", [System.Text.UTF8Encoding]::new($false))
$reportHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $reportPath).Hash.ToLowerInvariant()
$checksumPath = Join-Path $OutputDirectory "SHA256SUMS.txt"
[System.IO.File]::WriteAllText($checksumPath, "$reportHash  acceptance.json`n", [System.Text.UTF8Encoding]::new($false))

Write-Host "AUTOMATED_STATUS=$automatedStatus"
Write-Host "SOURCE_COMMIT=$candidateSha"
Write-Host "SOURCE_TREE=$treeSha"
Write-Host "EVIDENCE=$reportPath"
Write-Host "EVIDENCE_SHA256=$reportHash"
