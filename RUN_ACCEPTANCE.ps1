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
$EvidenceDir = Join-Path $Root "artifacts\acceptance\rg-r00-$RunId"
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null
$script:GateResults = [System.Collections.Generic.List[object]]::new()

function Add-Result([string]$Name, [string]$Status, [string]$Evidence) {
    $script:GateResults.Add([pscustomobject]@{
        name = $Name
        status = $Status
        evidence = $Evidence
    })
}

function Invoke-LoggedGate([string]$Name, [scriptblock]$Action) {
    $safeName = $Name.ToLowerInvariant() -replace '[^a-z0-9]+', '-'
    $logPath = Join-Path $EvidenceDir "$safeName.log"
    Write-Host ""
    Write-Host "=== $Name ==="
    $exitCode = 0
    try {
        & $Action *> $logPath
        if ($null -ne $LASTEXITCODE) {
            $exitCode = [int]$LASTEXITCODE
        }
    }
    catch {
        $_ | Out-String | Set-Content -LiteralPath $logPath -Encoding utf8
        $exitCode = 1
    }
    $status = if ($exitCode -eq 0) { "PASS" } else { "FAIL" }
    Add-Result $Name $status $logPath
    Get-Content -LiteralPath $logPath -Tail 18 | ForEach-Object { Write-Host $_ }
    Write-Host "RESULT=$status EXIT=$exitCode LOG=$logPath"
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
}

$baselinePath = "D:\MAX_JEV_BENCHMARK\JEV_TEV_EURUSD_H1_LAST2M"
$report = [ordered]@{
    schema_version = 1
    phase = "RG-R00"
    automated_status = "NOT_RUN"
    owner_ui_status = "OWNER_UI_ACCEPTANCE_PENDING"
    owner_acceptance = "PENDING"
    next_phase_unlocked = $false
    started_utc = $StartedUtc.ToString("o")
    completed_utc = [DateTime]::UtcNow.ToString("o")
    source = [ordered]@{
        root = $Root
        branch = $branch
        commit_sha = $commit
        tree_sha = $tree
        clean = $true
    }
    environment = [ordered]@{
        rust = $rustVersion
        node = $nodeVersion
        npm = $npmVersion
        msvc_ready = $msvcReady
        webview2_runtime = $webviewVersion
    }
    legacy_baseline = [ordered]@{
        path = $baselinePath
        accessed_by_runner = $false
        evidence_limit = "The runner does not inspect host state outside the repository and cannot attest to activity by other processes."
    }
    gates = @($script:GateResults)
}
$failed = @($script:GateResults | Where-Object status -eq "FAIL")
$blocked = @($script:GateResults | Where-Object status -eq "ENVIRONMENT_BLOCKED")
if ($failed.Count -gt 0) {
    $report.automated_status = "FAILED"
} elseif ($blocked.Count -gt 0) {
    $report.automated_status = "ENVIRONMENT_BLOCKED"
} else {
    $report.automated_status = "AUTOMATED_PASS"
}
$reportPath = Join-Path $EvidenceDir "acceptance.json"
$report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $reportPath -Encoding utf8
Write-Host ""
Write-Host "AUTOMATED_STATUS=$($report.automated_status)"
Write-Host "OWNER_UI_STATUS=$($report.owner_ui_status)"
Write-Host "OWNER_ACCEPTANCE=$($report.owner_acceptance)"
Write-Host "SOURCE_COMMIT=$commit"
Write-Host "SOURCE_TREE=$tree"
Write-Host "EVIDENCE=$reportPath"
if ($failed.Count -gt 0) { exit 1 }
if ($blocked.Count -gt 0) { exit 2 }
exit 0
