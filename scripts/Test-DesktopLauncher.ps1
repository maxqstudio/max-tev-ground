$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Desktop-LauncherFunctions.ps1")

$node = (Get-Command node.exe -ErrorAction Stop).Source
$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("rg-desktop-launcher-test-" + [Guid]::NewGuid().ToString("N"))
$commit = "a" * 40
$tree = "b" * 40
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null

function Assert-That([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function New-AcceptanceReport([string]$Schema, [string]$Phase, [string]$CommitSha, [string]$TreeSha, [bool]$Clean, [string]$AutomatedStatus, [string]$TauriStatus) {
    $tauriCheck = @{ name = "Tauri desktop build without packaging"; status = $TauriStatus; evidence = "fixture" }
    $source = @{ commit_sha = $CommitSha; tree_sha = $TreeSha; clean = $Clean }
    if ($Schema -eq "RG-R00") {
        return @{ phase = $Phase; automated_status = $AutomatedStatus; source = $source; gates = @($tauriCheck) }
    }
    # RG-R01 layout: gates are identified by id; the Tauri build is listed under local_execution.checks.
    return @{
        phase = $Phase
        automated_status = $AutomatedStatus
        source = $source
        gates = @(@{ id = "csv_ingestion"; status = "PASS"; evidence = "fixture" })
        local_execution = @{ checks = @(@{ name = "exact_source_clean"; status = "PASS" }, $tauriCheck) }
    }
}

function Find-InFixture([string]$Name, [hashtable]$Report, [switch]$WithExecutable, [string]$Commit = $commit, [string]$Tree = $tree) {
    $acceptanceRoot = Join-Path $fixtureRoot "$Name\acceptance"
    $directory = Join-Path $acceptanceRoot "fixture-run"
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $Report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $directory "acceptance.json") -Encoding utf8
    if ($WithExecutable) {
        $exeDirectory = Join-Path $directory "tauri-target\release"
        New-Item -ItemType Directory -Path $exeDirectory -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $exeDirectory "max-research-ground-desktop.exe") -Value "fixture" -Encoding ascii
    }
    return Find-AcceptedExecutable -AcceptanceRoot $acceptanceRoot -Commit $Commit -Tree $Tree
}

try {
    # Informative stderr with exit 0 is success; both streams must reach the log.
    $informativeLog = Join-Path $fixtureRoot "informative.log"
    $informativeExit = Invoke-LoggedNativeCommand -LogPath $informativeLog -Command {
        & $node -e "console.log('fixture stdout line'); console.error('Info fixture: informational stderr line'); process.exit(0)" 2>&1
    }
    Assert-That ($informativeExit -eq 0) "Informative stderr with exit 0 was reported as exit $informativeExit"
    $informativeText = Get-Content -LiteralPath $informativeLog -Raw
    Assert-That ($informativeText -match 'fixture stdout line') "Stdout was not preserved in the build log"
    Assert-That ($informativeText -match 'Info fixture: informational stderr line') "Informative stderr was not preserved in the build log"

    # A nonzero exit is failure even when it is the only signal; the exit code must come back unchanged.
    $failureLog = Join-Path $fixtureRoot "failure.log"
    $failureExit = Invoke-LoggedNativeCommand -LogPath $failureLog -Command {
        & $node -e "console.error('fixture failure line'); process.exit(17)" 2>&1
    }
    Assert-That ($failureExit -eq 17) "Nonzero native exit code was reported as $failureExit instead of 17"
    Assert-That ((Get-Content -LiteralPath $failureLog -Raw) -match 'fixture failure line') "Failure stderr was not preserved in the build log"

    # A command that never reports an exit code must not be treated as success.
    $silentLog = Join-Path $fixtureRoot "silent.log"
    $silentThrew = $false
    try {
        Invoke-LoggedNativeCommand -LogPath $silentLog -Command { Write-Output "no native process ran" } | Out-Null
    } catch {
        $silentThrew = $true
    }
    Assert-That $silentThrew "A command without a native exit code was accepted as success"

    # RG-R01 acceptance layout on the exact clean commit with a matching tree.
    $r01 = Find-InFixture "r01-exact" (New-AcceptanceReport "RG-R01" "RG-R01" $commit $tree $true "AUTOMATED_PASS" "PASS") -WithExecutable
    Assert-That ($null -ne $r01) "RG-R01 acceptance for the exact commit and tree was not accepted"
    Assert-That (Test-Path -LiteralPath $r01.Path -PathType Leaf) "Accepted RG-R01 executable path does not exist"

    # RG-R00 acceptance layout remains accepted.
    $r00 = Find-InFixture "r00-exact" (New-AcceptanceReport "RG-R00" "RG-R00" $commit $tree $true "AUTOMATED_PASS" "PASS") -WithExecutable
    Assert-That ($null -ne $r00) "RG-R00 acceptance for the exact commit was not accepted"

    # Every mismatch below must fail closed.
    $rejections = @(
        @{ Name = "other-commit"; Report = (New-AcceptanceReport "RG-R01" "RG-R01" ("c" * 40) $tree $true "AUTOMATED_PASS" "PASS") },
        @{ Name = "other-tree"; Report = (New-AcceptanceReport "RG-R01" "RG-R01" $commit ("d" * 40) $true "AUTOMATED_PASS" "PASS") },
        @{ Name = "dirty-source"; Report = (New-AcceptanceReport "RG-R01" "RG-R01" $commit $tree $false "AUTOMATED_PASS" "PASS") },
        @{ Name = "automated-failed"; Report = (New-AcceptanceReport "RG-R01" "RG-R01" $commit $tree $true "FAILED" "PASS") },
        @{ Name = "tauri-gate-failed"; Report = (New-AcceptanceReport "RG-R01" "RG-R01" $commit $tree $true "AUTOMATED_PASS" "FAIL") },
        @{ Name = "unlisted-phase"; Report = (New-AcceptanceReport "RG-R01" "RG-R02" $commit $tree $true "AUTOMATED_PASS" "PASS") }
    )
    foreach ($case in $rejections) {
        $found = Find-InFixture $case.Name $case.Report -WithExecutable
        Assert-That ($null -eq $found) "Acceptance evidence with mismatched $($case.Name) was accepted"
    }
    $missingExe = Find-InFixture "missing-executable" (New-AcceptanceReport "RG-R01" "RG-R01" $commit $tree $true "AUTOMATED_PASS" "PASS")
    Assert-That ($null -eq $missingExe) "Acceptance evidence without an executable was accepted"
    $wrongCommit = Find-InFixture "lookup-wrong-commit" (New-AcceptanceReport "RG-R01" "RG-R01" $commit $tree $true "AUTOMATED_PASS" "PASS") -WithExecutable -Commit ("e" * 40)
    Assert-That ($null -eq $wrongCommit) "A lookup for a different commit returned an executable"

    Write-Host "DESKTOP_LAUNCHER_TESTS=PASS"
} finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}
