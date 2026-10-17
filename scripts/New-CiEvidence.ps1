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
    "source_check", "evidence_contract_tests", "rust_fmt", "roadmap_governance", "rust_tests", "rust_clippy", "rust_build",
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
$testStatus = Get-GateStatus @($steps["rust_tests"])

$gates = @(
    [pscustomobject]@{
        id = "rust_workspace"
        status = $rustStatus
        evidence = @($rustSteps | ForEach-Object { New-StepEvidence $_ $steps[$_] })
    },
    [pscustomobject]@{
        id = "frontend"
        status = $frontendStatus
        evidence = @($frontendSteps | ForEach-Object { New-StepEvidence $_ $steps[$_] })
    },
    [pscustomobject]@{
        id = "tauri_build"
        status = $tauriStatus
        evidence = @(
            (New-StepEvidence "msvc_import" $steps["msvc_import"]),
            (New-StepEvidence "tauri_build" $steps["tauri_build"])
        )
    },
    [pscustomobject]@{
        id = "persistence"
        status = $testStatus
        evidence = @(New-StepEvidence "rust_tests" $steps["rust_tests"])
    },
    [pscustomobject]@{
        id = "contracts"
        status = $testStatus
        evidence = @(New-StepEvidence "rust_tests" $steps["rust_tests"])
    },
    [pscustomobject]@{
        id = "roadmap_governance"
        status = $roadmapStatus
        evidence = @(New-StepEvidence "roadmap_governance" $steps["roadmap_governance"])
    },
    [pscustomobject]@{
        id = "baseline_read_only"
        status = "NOT_VERIFIED"
        evidence = "The GitHub runner has no access to the Owner host's legacy baseline; this report makes no claim about that host state."
    },
    [pscustomobject]@{
        id = "exact_source_evidence"
        status = $exactSourceStatus
        evidence = [pscustomobject]@{
            event_sha = $expectedSha
            checked_out_sha = $candidateSha
            tree_sha = $treeSha
            clean_checkout = $sourceIsClean
        }
    },
    [pscustomobject]@{
        id = "owner_ui_acceptance"
        status = "OWNER_UI_ACCEPTANCE_PENDING"
        evidence = "GitHub Actions does not launch or interact with the Owner's desktop UI."
    }
)

$automatedGates = @($rustStatus, $frontendStatus, $roadmapStatus, $tauriStatus, $evidenceContractStatus)
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
    phase = "RG-R00"
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
    owner_ui_status = "OWNER_UI_ACCEPTANCE_PENDING"
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
