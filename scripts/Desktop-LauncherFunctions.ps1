# Helpers shared by RUN_DESKTOP.ps1 and Test-DesktopLauncher.ps1. Dot-source this file; it defines functions only.

function Invoke-LoggedNativeCommand {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Command,
        [Parameter(Mandatory = $true)][string]$LogPath
    )
    # Windows PowerShell 5.1 turns each stderr line of a native command into an ErrorRecord. Under
    # ErrorActionPreference=Stop the first record terminates the pipeline before the native exit code
    # is read. Success is therefore decided from $LASTEXITCODE, while stdout and stderr are both logged.
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        $global:LASTEXITCODE = $null
        & $Command | Tee-Object -FilePath $LogPath -Append | Out-Host
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($null -eq $global:LASTEXITCODE) { throw "The native command did not report an exit code. Log: $LogPath" }
    return [int]$global:LASTEXITCODE
}

function Find-AcceptedExecutable {
    param(
        [Parameter(Mandatory = $true)][string]$AcceptanceRoot,
        [Parameter(Mandatory = $true)][string]$Commit,
        [Parameter(Mandatory = $true)][string]$Tree
    )
    # Phases whose exact-commit acceptance evidence may supply the desktop binary.
    $acceptedPhases = @("RG-R00", "RG-R01")
    if (-not (Test-Path -LiteralPath $AcceptanceRoot -PathType Container)) { return $null }

    foreach ($directory in (Get-ChildItem -LiteralPath $AcceptanceRoot -Directory | Sort-Object LastWriteTimeUtc -Descending)) {
        $reportPath = Join-Path $directory.FullName "acceptance.json"
        if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { continue }
        try { $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json }
        catch { continue }

        # RG-R00 reports name every gate. RG-R01 reports identify gates by id and record the Tauri build in local_execution.checks.
        $tauriBuilds = @(@($report.gates) + @($report.local_execution.checks) |
            Where-Object { $_.name -eq "Tauri desktop build without packaging" -and $_.status -eq "PASS" })
        if ($acceptedPhases -notcontains $report.phase -or
            $report.automated_status -ne "AUTOMATED_PASS" -or
            $report.source.commit_sha -ne $Commit -or
            $report.source.tree_sha -ne $Tree -or
            $report.source.clean -ne $true -or
            $tauriBuilds.Count -ne 1) { continue }

        $exe = Join-Path $directory.FullName "tauri-target\release\max-research-ground-desktop.exe"
        if (Test-Path -LiteralPath $exe -PathType Leaf) {
            return [pscustomobject]@{ Path = $exe; Report = $reportPath; Commit = $Commit }
        }
    }
    return $null
}
