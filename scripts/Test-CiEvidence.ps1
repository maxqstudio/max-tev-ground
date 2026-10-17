$ErrorActionPreference = "Stop"
$root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..")).TrimEnd("\")
$generator = Join-Path $PSScriptRoot "New-CiEvidence.ps1"
$fixtureRoot = Join-Path $root ("work\ci-evidence-test-" + [Guid]::NewGuid().ToString("N"))
$sourceSha = (& git -C $root rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceSha -notmatch '^[0-9a-f]{40,64}$') {
    throw "Cannot resolve a source SHA for CI evidence tests"
}

$environmentNames = @(
    "GITHUB_WORKSPACE", "GITHUB_SHA", "GITHUB_REPOSITORY", "GITHUB_REF", "GITHUB_REF_NAME",
    "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_EVENT_NAME", "GITHUB_WORKFLOW",
    "RG_STEP_source_check", "RG_STEP_evidence_contract_tests", "RG_STEP_rust_fmt", "RG_STEP_roadmap_governance", "RG_STEP_rust_tests",
    "RG_STEP_rust_clippy", "RG_STEP_rust_build", "RG_STEP_reproducible_acceptance", "RG_STEP_npm_ci", "RG_STEP_frontend_lint",
    "RG_STEP_frontend_typecheck", "RG_STEP_frontend_tests", "RG_STEP_frontend_build",
    "RG_STEP_msvc_import", "RG_STEP_tauri_build"
)
$phaseText = Get-Content -LiteralPath (Join-Path $root "PHASE_MANIFEST.yaml") -Raw
$gateSection = [regex]::Match($phaseText, '(?ms)^acceptance_gates:\s*(.*?)(?=^\S|\z)')
if (-not $gateSection.Success) { throw "Could not read phase acceptance gates" }
$gateIds = @([regex]::Matches($gateSection.Groups[1].Value, '(?m)^\s+- id:\s*([a-z0-9_]+)\s*$') | ForEach-Object { $_.Groups[1].Value })
foreach ($id in $gateIds) { $environmentNames += "RG_GATE_$id" }
$previousValues = @{}
foreach ($name in $environmentNames) {
    $previousValues[$name] = [Environment]::GetEnvironmentVariable($name)
}

try {
    $env:GITHUB_WORKSPACE = $root
    $env:GITHUB_SHA = $sourceSha
    $env:GITHUB_REPOSITORY = "maxqstudio/max-tev-ground"
    $env:GITHUB_REF = "refs/heads/work/rg-r01-data-ground"
    $env:GITHUB_REF_NAME = "work/rg-r01-data-ground"
    $env:GITHUB_RUN_ID = "fixture-run"
    $env:GITHUB_RUN_ATTEMPT = "1"
    $env:GITHUB_EVENT_NAME = "push"
    $env:GITHUB_WORKFLOW = "MAX Research Ground CI"

    $stepNames = @(
        "source_check", "evidence_contract_tests", "rust_fmt", "roadmap_governance", "reproducible_acceptance", "rust_tests", "rust_clippy", "rust_build",
        "npm_ci", "frontend_lint", "frontend_typecheck", "frontend_tests", "frontend_build", "msvc_import", "tauri_build"
    )
    foreach ($name in $stepNames) { [Environment]::SetEnvironmentVariable("RG_STEP_$name", "success") }
    foreach ($id in $gateIds) {
        if ($id -ne "owner_ui_acceptance") { [Environment]::SetEnvironmentVariable("RG_GATE_$id", "success") }
    }

    $passingDirectory = Join-Path $fixtureRoot "passing"
    & $generator -OutputDirectory $passingDirectory
    $passingPath = Join-Path $passingDirectory "acceptance.json"
    $passing = Get-Content -LiteralPath $passingPath -Raw | ConvertFrom-Json
    if ($passing.automated_status -ne "AUTOMATED_PASS") { throw "Passing fixture did not report AUTOMATED_PASS" }
    if ($passing.phase -ne "RG-R01") { throw "Acceptance report phase is not synchronized with the active phase" }
    if ($passing.gates.Count -ne $gateIds.Count) { throw "Acceptance report omitted a declared phase gate" }
    if ($passing.source.commit_sha -ne $sourceSha -or -not $passing.source.exact_event_match) { throw "Acceptance report did not bind to the exact event SHA" }
    if ($passing.source.tree_sha -notmatch '^[0-9a-f]{40,64}$') { throw "Acceptance report omitted the Git tree SHA" }
    if ($passing.workflow.run_id -ne "fixture-run") { throw "Acceptance report omitted workflow run identity" }
    if (($passing.gates | Where-Object id -eq "reproducible_acceptance").status -ne "PASS") { throw "Exact source/governance gate did not pass" }
    if ($passing.legacy_baseline_status -ne "NOT_VERIFIED_BY_GITHUB_CI") { throw "Legacy baseline status was overstated" }
    if ($passing.owner_ui_status -ne "OWNER_UI_ACCEPTANCE_PENDING") { throw "Owner UI review was not left pending" }
    if (($passing.gates | Where-Object id -eq "owner_ui_acceptance").status -ne "OWNER_UI_ACCEPTANCE_PENDING") { throw "Owner UI gate was not left pending" }
    $checksumLine = (Get-Content -LiteralPath (Join-Path $passingDirectory "SHA256SUMS.txt") -Raw).Trim()
    $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $passingPath).Hash.ToLowerInvariant()
    if ($checksumLine -ne "$actualHash  acceptance.json") { throw "Acceptance evidence checksum is invalid" }

    [Environment]::SetEnvironmentVariable("RG_STEP_rust_tests", "failure")
    $failingDirectory = Join-Path $fixtureRoot "failing"
    & $generator -OutputDirectory $failingDirectory
    $failing = Get-Content -LiteralPath (Join-Path $failingDirectory "acceptance.json") -Raw | ConvertFrom-Json
    if ($failing.automated_status -ne "FAILED") { throw "Failed fixture was not reported as FAILED" }
    [Environment]::SetEnvironmentVariable("RG_STEP_rust_tests", "success")
    [Environment]::SetEnvironmentVariable("RG_GATE_csv_ingestion", "failure")
    $gateFailureDirectory = Join-Path $fixtureRoot "gate-failure"
    & $generator -OutputDirectory $gateFailureDirectory
    $gateFailure = Get-Content -LiteralPath (Join-Path $gateFailureDirectory "acceptance.json") -Raw | ConvertFrom-Json
    if ($gateFailure.automated_status -ne "FAILED") { throw "A failed phase gate was not reported as FAILED" }
    if (($gateFailure.gates | Where-Object id -eq "csv_ingestion").status -ne "FAIL") { throw "A failed CSV gate was not represented accurately" }

    [Environment]::SetEnvironmentVariable("RG_GATE_csv_ingestion", "success")
    [Environment]::SetEnvironmentVariable("RG_STEP_frontend_tests", "skipped")
    $skippedDirectory = Join-Path $fixtureRoot "skipped"
    & $generator -OutputDirectory $skippedDirectory
    $skipped = Get-Content -LiteralPath (Join-Path $skippedDirectory "acceptance.json") -Raw | ConvertFrom-Json
    if (($skipped.gates | Where-Object id -eq "frontend").status -ne "NOT_RUN") { throw "Skipped frontend gate was presented as passing" }

    Write-Host "CI_EVIDENCE_CONTRACT_TESTS=PASS"
} finally {
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $previousValues[$name])
    }
    $resolvedFixtureRoot = [System.IO.Path]::GetFullPath($fixtureRoot).TrimEnd("\")
    $allowedPrefix = (Join-Path $root "work\ci-evidence-test-")
    if ($resolvedFixtureRoot.StartsWith($allowedPrefix, [System.StringComparison]::OrdinalIgnoreCase) -and
        (Test-Path -LiteralPath $resolvedFixtureRoot)) {
        Remove-Item -LiteralPath $resolvedFixtureRoot -Recurse -Force
    }
}
