param()

$ErrorActionPreference = "Stop"
$ExpectedRoot = "D:\MAX_RESEARCH_GROUND"
$Root = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd("\")
if ($Root -ne $ExpectedRoot) {
    throw "Acceptance runner must execute from $ExpectedRoot; found $Root"
}
Set-Location -LiteralPath $Root

$StartedUtc = [DateTime]::UtcNow
$RunId = $StartedUtc.ToString("yyyyMMddTHHmmssZ")
$EvidenceDir = Join-Path $Root "artifacts\acceptance\rg-r01-$RunId"
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null
$script:GateResults = [System.Collections.Generic.List[object]]::new()
$script:StepOutcomes = [ordered]@{}

function Add-Result([string]$Name, [string]$Status, [string]$Evidence) {
    $script:GateResults.Add([pscustomobject]@{
        name = $Name
        status = $Status
        evidence = $Evidence
    })
}

function Invoke-LoggedGate([string]$Name, [scriptblock]$Action, [int]$ExpectedExitCode = 0) {
    $safeName = $Name.ToLowerInvariant() -replace '[^a-z0-9]+', '-'
    $logPath = Join-Path $EvidenceDir "$safeName.log"
    Write-Host ""
    Write-Host "=== $Name ==="
    $exitCode = 0
    $previousErrorActionPreference = $ErrorActionPreference
    $previousErrorCount = $global:Error.Count
    try {
        # Native tools may write successful progress or diagnostics to stderr. Capture those
        # records in the gate log and decide success from the process exit code instead.
        $ErrorActionPreference = "Continue"
        $global:LASTEXITCODE = $null
        & $Action *> $logPath
        if ($null -ne $LASTEXITCODE) {
            $exitCode = [int]$LASTEXITCODE
        }
        $newErrorCount = [Math]::Max(0, $global:Error.Count - $previousErrorCount)
        $actionErrors = @($global:Error | Select-Object -First $newErrorCount)
        if (@($actionErrors | Where-Object { $_.FullyQualifiedErrorId -notlike "NativeCommandError*" }).Count -gt 0) {
            $exitCode = 1
        }
    }
    catch {
        $_ | Out-String | Set-Content -LiteralPath $logPath -Encoding utf8
        $exitCode = 1
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    $status = if ($exitCode -eq $ExpectedExitCode) { "PASS" } else { "FAIL" }
    Add-Result $Name $status $logPath
    $stepName = switch ($Name) {
        "Rust formatting" { "rust_fmt" }
        "Roadmap governance" { "roadmap_governance" }
        "CI evidence contract tests" { "evidence_contract_tests" }
        "Rust workspace tests" { "rust_tests" }
        "Rust lint" { "rust_clippy" }
        "Rust workspace build" { "rust_build" }
        "Frontend clean install" { "npm_ci" }
        "Frontend lint" { "frontend_lint" }
        "Frontend typecheck" { "frontend_typecheck" }
        "Frontend unit tests" { "frontend_tests" }
        "Frontend production build" { "frontend_build" }
        "Tauri desktop build without packaging" { "tauri_build" }
        default { $null }
    }
    if ($stepName) {
        $outcome = if ($status -eq "PASS") { "success" } else { "failure" }
        $script:StepOutcomes[$stepName] = $outcome
        [Environment]::SetEnvironmentVariable("RG_STEP_$stepName", $outcome)
    }
    Get-Content -LiteralPath $logPath -Tail 18 | ForEach-Object { Write-Host $_ }
    Write-Host "RESULT=$status EXIT=$exitCode EXPECTED=$ExpectedExitCode LOG=$logPath"
}

$commit = (git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -notmatch '^[0-9a-f]{40,64}$') {
    throw "Cannot resolve exact source commit SHA"
}
$tree = (git rev-parse 'HEAD^{tree}').Trim()
if ($LASTEXITCODE -ne 0 -or $tree -notmatch '^[0-9a-f]{40,64}$') {
    throw "Cannot resolve exact Git tree SHA"
}
$branch = (git rev-parse --abbrev-ref HEAD).Trim()
$expectedBranch = "work/rg-r01-data-ground"
if ($branch -ne $expectedBranch) {
    Add-Result "authorized_branch" "FAIL" "expected=$expectedBranch actual=$branch"
    throw "Acceptance must run on $expectedBranch; found $branch"
}
$script:StepOutcomes["source_check"] = "success"
[Environment]::SetEnvironmentVariable("RG_STEP_source_check", "success")
$dirty = @(git status --porcelain --untracked-files=normal)
if ($LASTEXITCODE -ne 0) {
    throw "Cannot inspect Git status"
}
if ($dirty.Count -gt 0) {
    $dirtyPath = Join-Path $EvidenceDir "dirty-source.txt"
    $dirty | Set-Content -LiteralPath $dirtyPath -Encoding utf8
    Add-Result "exact_source_clean" "FAIL" $dirtyPath
    throw "Acceptance requires a clean candidate commit; see $dirtyPath"
}
Add-Result "exact_source_clean" "PASS" "commit=$commit tree=$tree branch=$branch"

$RUSTUP_HOME = Join-Path $Root ".tools\rustup"
$CARGO_HOME = Join-Path $Root ".tools\cargo"
$env:RUSTUP_HOME = $RUSTUP_HOME
$env:CARGO_HOME = $CARGO_HOME
$env:PATH = "$CARGO_HOME\bin;$env:PATH"
& (Join-Path $Root "scripts\Initialize-Toolchain.ps1")

$nodeVersion = if (Get-Command node -ErrorAction SilentlyContinue) { (& node --version).Trim() } else { "MISSING" }
$npmVersion = if (Get-Command npm -ErrorAction SilentlyContinue) { (& npm --version).Trim() } else { "MISSING" }
$rustVersion = (& rustc --version).Trim()
if ($nodeVersion -ne "v24.19.0" -or $npmVersion -ne "11.17.0" -or $rustVersion -notmatch '^rustc 1\.99\.1 ') {
    Add-Result "pinned_toolchains" "ENVIRONMENT_BLOCKED" "node=$nodeVersion npm=$npmVersion rust=$rustVersion; required node=v24.19.0 npm=11.17.0 rustc=1.99.1"
} else {
    Add-Result "pinned_toolchains" "PASS" "node=$nodeVersion npm=$npmVersion rust=$rustVersion"
}
$nodeCommand = Get-Command node -ErrorAction SilentlyContinue
if ($null -ne $nodeCommand) {
    Invoke-LoggedGate "Acceptance stderr handling self-test" {
        & $nodeCommand.Source -e "console.error('fixture: successful command wrote to stderr'); process.exit(0)"
    }
    Invoke-LoggedGate "Acceptance nonzero exit self-test" {
        & $nodeCommand.Source -e "console.error('fixture: expected command failure'); process.exit(17)"
    } 17
} else {
    Add-Result "Acceptance stderr handling self-test" "ENVIRONMENT_BLOCKED" "Node.js is required to verify the native process stderr and exit-code handling"
    Add-Result "Acceptance nonzero exit self-test" "ENVIRONMENT_BLOCKED" "Node.js is required to verify the native process stderr and exit-code handling"
}

$programFilesX86 = [System.Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
$vswhere = Join-Path $programFilesX86 "Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = $null
if (Test-Path -LiteralPath $vswhere) {
    $vsPath = (& $vswhere -latest -products "*" -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
}
$msvcReady = $false
if ($vsPath) {
    $vsDevCmd = Join-Path $vsPath "Common7\Tools\VsDevCmd.bat"
    if (Test-Path -LiteralPath $vsDevCmd) {
        $quote = [char]34
        $commandLine = "call $quote$vsDevCmd$quote -no_logo -arch=x64 -host_arch=x64 >nul && set"
        $environmentLines = & $env:ComSpec /d /s /c $commandLine
        if ($LASTEXITCODE -eq 0) {
            foreach ($line in $environmentLines) {
                $separator = $line.IndexOf("=")
                if ($separator -gt 0) {
                    $name = $line.Substring(0, $separator)
                    $value = $line.Substring($separator + 1)
                    Set-Item -Path "Env:$name" -Value $value
                }
            }
            $msvcReady = [bool](Get-Command cl.exe -ErrorAction SilentlyContinue)
        }
    }
}
if ($msvcReady) {
    Add-Result "windows_msvc_prerequisite" "PASS" "Visual Studio Build Tools at $vsPath; VCToolsVersion=$env:VCToolsVersion; WindowsSDKVersion=$env:WindowsSDKVersion"
    $script:StepOutcomes["msvc_import"] = "success"
    [Environment]::SetEnvironmentVariable("RG_STEP_msvc_import", "success")
} else {
    Add-Result "windows_msvc_prerequisite" "ENVIRONMENT_BLOCKED" "MSVC C++ Build Tools x64 developer environment was not available"
}

$webviewRoot = Join-Path $programFilesX86 "Microsoft\EdgeWebView\Application"
$webviewVersion = $null
if (Test-Path -LiteralPath $webviewRoot) {
    $runtime = Get-ChildItem -LiteralPath $webviewRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
        Sort-Object { [version]$_.Name } -Descending |
        Select-Object -First 1
    if ($runtime) { $webviewVersion = $runtime.Name }
}
if ($webviewVersion) {
    Add-Result "webview2_runtime" "PASS" "WebView2 runtime $webviewVersion at $webviewRoot"
} else {
    Add-Result "webview2_runtime" "ENVIRONMENT_BLOCKED" "WebView2 Runtime was not found at $webviewRoot; the app was not launched"
}

Invoke-LoggedGate "Rust formatting" { cargo fmt --all -- --check }
Invoke-LoggedGate "Roadmap governance" { cargo run --locked -p rg-governance --bin rg-governance -- --check }
Invoke-LoggedGate "CI evidence contract tests" {
    & (Join-Path $Root "scripts\Test-CiEvidence.ps1")
    $global:LASTEXITCODE = 0
}
Invoke-LoggedGate "Rust workspace tests" { cargo test --workspace --locked }
Invoke-LoggedGate "Rust lint" { cargo clippy --workspace --all-targets --locked -- -D warnings }
Invoke-LoggedGate "Rust workspace build" { cargo build --workspace --locked }

Invoke-LoggedGate "Frontend clean install" { npm ci --no-audit --no-fund }
Invoke-LoggedGate "Frontend lint" { npm run lint }
Invoke-LoggedGate "Frontend typecheck" { npm run typecheck }
Invoke-LoggedGate "Frontend unit tests" { npm test -- --run }
Invoke-LoggedGate "Frontend production build" { npm run build }

if ($msvcReady) {
    Invoke-LoggedGate "Tauri desktop build without packaging" {
        $previousCargoTarget = $env:CARGO_TARGET_DIR
        try {
            $env:CARGO_TARGET_DIR = Join-Path $EvidenceDir "tauri-target"
            npm run tauri -- build --no-bundle
        }
        finally {
            if ($null -eq $previousCargoTarget) {
                Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
            } else {
                $env:CARGO_TARGET_DIR = $previousCargoTarget
            }
        }
    }
} else {
    Add-Result "Tauri desktop build without packaging" "ENVIRONMENT_BLOCKED" "Requires the MSVC x64 developer environment"
    $script:StepOutcomes["msvc_import"] = "ENVIRONMENT_BLOCKED"
    $script:StepOutcomes["tauri_build"] = "ENVIRONMENT_BLOCKED"
    [Environment]::SetEnvironmentVariable("RG_STEP_msvc_import", "ENVIRONMENT_BLOCKED")
    [Environment]::SetEnvironmentVariable("RG_STEP_tauri_build", "ENVIRONMENT_BLOCKED")
}

$env:GITHUB_WORKSPACE = $Root
$env:GITHUB_SHA = $commit
$env:GITHUB_REPOSITORY = "maxqstudio/max-tev-ground"
$env:GITHUB_REF = "refs/heads/$branch"
$env:GITHUB_REF_NAME = $branch
$env:GITHUB_RUN_ID = "local-$RunId"
$env:GITHUB_RUN_ATTEMPT = "1"
$env:GITHUB_EVENT_NAME = "local-acceptance"
$env:GITHUB_WORKFLOW = "MAX Research Ground Local Acceptance"
$phaseText = Get-Content -LiteralPath (Join-Path $Root "PHASE_MANIFEST.yaml") -Raw
$gateSection = [regex]::Match($phaseText, '(?ms)^acceptance_gates:[ \t]*\r?\n(.*?)(?=^\S|\z)')
if (-not $gateSection.Success) { throw "Cannot resolve active phase acceptance gates for local evidence" }
$gateIds = @([regex]::Matches($gateSection.Groups[1].Value, '(?m)^\s+- id:\s*([a-z0-9_]+)\s*$') | ForEach-Object { $_.Groups[1].Value })
foreach ($gateId in $gateIds) { [Environment]::SetEnvironmentVariable("RG_GATE_$gateId", $null) }
$env:RG_STEP_reproducible_acceptance = if ($script:StepOutcomes["roadmap_governance"] -eq "success" -and $script:StepOutcomes["source_check"] -eq "success") { "success" } else { "failure" }
$script:StepOutcomes["reproducible_acceptance"] = $env:RG_STEP_reproducible_acceptance
$global:LASTEXITCODE = $null
& (Join-Path $Root "scripts\New-CiEvidence.ps1") -OutputDirectory $EvidenceDir
$reportPath = Join-Path $EvidenceDir "acceptance.json"
if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
    throw "Could not generate source-bound acceptance evidence"
}
$report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
if ($report.source.commit_sha -ne $commit -or $report.source.tree_sha -ne $tree) {
    throw "Generated acceptance evidence does not match the tested source commit and tree"
}
$report | Add-Member -NotePropertyName local_execution -NotePropertyValue ([ordered]@{
    started_utc = $StartedUtc.ToString("o")
    completed_utc = [DateTime]::UtcNow.ToString("o")
    root = $Root
    branch = $branch
    environment = [ordered]@{
        rust = $rustVersion
        node = $nodeVersion
        npm = $npmVersion
        msvc_ready = $msvcReady
        webview2_runtime = $webviewVersion
    }
    checks = @($script:GateResults)
}) -Force
$localFailures = @($script:GateResults | Where-Object status -eq "FAIL")
$localBlocks = @($script:GateResults | Where-Object status -eq "ENVIRONMENT_BLOCKED")
if ($localFailures.Count -gt 0) {
    $report.automated_status = "FAILED"
} elseif ($localBlocks.Count -gt 0 -and $report.automated_status -eq "AUTOMATED_PASS") {
    $report.automated_status = "ENVIRONMENT_BLOCKED"
}
$json = $report | ConvertTo-Json -Depth 16
[System.IO.File]::WriteAllText($reportPath, $json + "`n", [System.Text.UTF8Encoding]::new($false))
$reportHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $reportPath).Hash.ToLowerInvariant()
$checksumPath = Join-Path $EvidenceDir "SHA256SUMS.txt"
[System.IO.File]::WriteAllText($checksumPath, "$reportHash  acceptance.json`n", [System.Text.UTF8Encoding]::new($false))
$failed = @($script:GateResults | Where-Object status -eq "FAIL")
$blocked = @($script:GateResults | Where-Object status -eq "ENVIRONMENT_BLOCKED")
Write-Host ""
Write-Host "AUTOMATED_STATUS=$($report.automated_status)"
Write-Host "OWNER_UI_STATUS=$($report.owner_ui_status)"
Write-Host "SOURCE_COMMIT=$commit"
Write-Host "SOURCE_TREE=$tree"
Write-Host "EVIDENCE=$reportPath"
Write-Host "EVIDENCE_SHA256=$reportHash"
if ($report.automated_status -eq "FAILED" -or $failed.Count -gt 0) { exit 1 }
if ($report.automated_status -eq "ENVIRONMENT_BLOCKED" -or $blocked.Count -gt 0) { exit 2 }
if ($report.automated_status -ne "AUTOMATED_PASS") { exit 3 }
exit 0
