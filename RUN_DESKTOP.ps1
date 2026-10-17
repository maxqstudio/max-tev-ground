[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "Low")]
param()

$ErrorActionPreference = "Stop"
$ExpectedRoot = "D:\MAX_RESEARCH_GROUND"
$Root = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd("\")
$RunnerLog = $null

function Get-SourceFingerprint {
    $files = @(& git -C $Root ls-files --cached --others --exclude-standard)
    if ($LASTEXITCODE -ne 0) { throw "Could not enumerate source files with Git." }

    $entries = [System.Collections.Generic.List[string]]::new()
    foreach ($relativePath in ($files | Sort-Object -Unique)) {
        if ([string]::IsNullOrWhiteSpace($relativePath)) { continue }
        $absolutePath = Join-Path $Root $relativePath
        if (Test-Path -LiteralPath $absolutePath -PathType Leaf) {
            $fileHash = (Get-FileHash -LiteralPath $absolutePath -Algorithm SHA256).Hash
            $entries.Add("$relativePath|$fileHash")
        } else {
            $entries.Add("$relativePath|MISSING")
        }
    }

    $bytes = [System.Text.Encoding]::UTF8.GetBytes([string]::Join("`n", $entries))
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([System.BitConverter]::ToString($sha.ComputeHash($bytes))).Replace("-", "") }
    finally { $sha.Dispose() }
}

function Find-AcceptedExecutable([string]$Commit) {
    $acceptanceRoot = Join-Path $Root "artifacts\acceptance"
    if (-not (Test-Path -LiteralPath $acceptanceRoot -PathType Container)) { return $null }

    foreach ($directory in (Get-ChildItem -LiteralPath $acceptanceRoot -Directory | Sort-Object LastWriteTimeUtc -Descending)) {
        $reportPath = Join-Path $directory.FullName "acceptance.json"
        if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { continue }
        try { $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json }
        catch { continue }

        $tauriGate = @($report.gates | Where-Object { $_.name -eq "Tauri desktop build without packaging" -and $_.status -eq "PASS" })
        if ($report.phase -ne "RG-R00" -or
            $report.automated_status -ne "AUTOMATED_PASS" -or
            $report.source.commit_sha -ne $Commit -or
            $report.source.clean -ne $true -or
            $tauriGate.Count -ne 1) { continue }

        $exe = Join-Path $directory.FullName "tauri-target\release\max-research-ground-desktop.exe"
        if (Test-Path -LiteralPath $exe -PathType Leaf) {
            return [pscustomobject]@{ Path = $exe; Report = $reportPath; Commit = $Commit }
        }
    }
    return $null
}

function Import-MsvcEnvironment {
    $programFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
    if ([string]::IsNullOrWhiteSpace($programFilesX86)) { throw "The Windows x64 MSVC build environment is unavailable." }
    $vswhere = Join-Path $programFilesX86 "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { throw "Visual Studio Build Tools were not found. Install the MSVC C++ x64 build tools to rebuild the desktop app." }

    $vsPath = (& $vswhere -latest -products "*" -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($vsPath)) { throw "Visual Studio Build Tools with the MSVC x64 component were not found." }
    $vsDevCmd = Join-Path $vsPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $vsDevCmd -PathType Leaf)) { throw "VsDevCmd.bat is missing from the detected Visual Studio Build Tools installation." }

    $quote = [char]34
    $commandLine = "call $quote$vsDevCmd$quote -no_logo -arch=x64 -host_arch=x64 >nul && set"
    $environmentLines = & $env:ComSpec /d /s /c $commandLine
    if ($LASTEXITCODE -ne 0) { throw "Could not initialize the MSVC x64 developer environment." }
    foreach ($line in $environmentLines) {
        $separator = $line.IndexOf("=")
        if ($separator -gt 0) {
            $name = $line.Substring(0, $separator)
            $value = $line.Substring($separator + 1)
            Set-Item -Path "Env:$name" -Value $value
        }
    }
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) { throw "MSVC initialization completed without exposing cl.exe." }
}

try {
    if ($Root -ne $ExpectedRoot) { throw "Desktop runner is bound to $ExpectedRoot; found $Root." }
    Set-Location -LiteralPath $Root

    $head = (& git -C $Root rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $head -notmatch '^[0-9a-f]{40,64}$') { throw "Could not resolve the current Git commit." }
    $tree = (& git -C $Root rev-parse 'HEAD^{tree}').Trim()
    if ($LASTEXITCODE -ne 0 -or $tree -notmatch '^[0-9a-f]{40,64}$') { throw "Could not resolve the current Git tree." }
    $sourceFingerprint = Get-SourceFingerprint

    if (Get-Process -Name "max-research-ground-desktop" -ErrorAction SilentlyContinue) {
        Write-Host "MAX Research Ground is already running. Switch to its existing window."
        return
    }

    $programFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
    $webviewRoot = if ($programFilesX86) { Join-Path $programFilesX86 "Microsoft\EdgeWebView\Application" } else { $null }
    $webviewRuntime = if ($webviewRoot -and (Test-Path -LiteralPath $webviewRoot)) {
        Get-ChildItem -LiteralPath $webviewRoot -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
            Sort-Object { [version]$_.Name } -Descending |
            Select-Object -First 1
    } else { $null }
    if (-not $webviewRuntime) { throw "WebView2 Runtime was not found. Install the Evergreen WebView2 Runtime, then retry." }

    $exe = $null
    $accepted = $null
    $status = @(& git -C $Root status --porcelain --untracked-files=normal)
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect the source worktree status." }
    if ($status.Count -eq 0) {
        $accepted = Find-AcceptedExecutable -Commit $head
        if ($accepted) { $exe = $accepted.Path }
    }

    $runnerRoot = Join-Path $Root "artifacts\desktop-runner"
    $targetRoot = Join-Path $runnerRoot "cargo-target"
    $cachedExe = Join-Path $targetRoot "release\max-research-ground-desktop.exe"
    $cacheManifestPath = Join-Path $runnerRoot "build.json"
    if (-not $exe -and (Test-Path -LiteralPath $cacheManifestPath -PathType Leaf) -and (Test-Path -LiteralPath $cachedExe -PathType Leaf)) {
        try { $cacheManifest = Get-Content -LiteralPath $cacheManifestPath -Raw | ConvertFrom-Json }
        catch { $cacheManifest = $null }
        if ($cacheManifest -and
            $cacheManifest.commit_sha -eq $head -and
            $cacheManifest.tree_sha -eq $tree -and
            $cacheManifest.source_fingerprint -eq $sourceFingerprint -and
            $cacheManifest.executable_sha256 -eq (Get-FileHash -LiteralPath $cachedExe -Algorithm SHA256).Hash) {
            $exe = $cachedExe
            Write-Host "Using the verified local desktop build for source fingerprint $sourceFingerprint."
        }
    }

    if (-not $exe) {
        $exe = $cachedExe
        $buildDescription = "Build MAX Research Ground from the current source into $targetRoot"
        if ($WhatIfPreference) {
            Write-Host "What if: $buildDescription"
        } elseif ($PSCmdlet.ShouldProcess($exe, $buildDescription)) {
            $cargoHome = Join-Path $Root ".tools\cargo"
            $rustupHome = Join-Path $Root ".tools\rustup"
            $env:CARGO_HOME = $cargoHome
            $env:RUSTUP_HOME = $rustupHome
            $env:PATH = "$(Join-Path $cargoHome 'bin');$env:PATH"

            $cargoExe = Join-Path $cargoHome "bin\cargo.exe"
            $rustcExe = Join-Path $cargoHome "bin\rustc.exe"
            if (-not (Test-Path -LiteralPath $cargoExe -PathType Leaf) -or -not (Test-Path -LiteralPath $rustcExe -PathType Leaf)) {
                throw "The project-local Rust toolchain is missing. Run .\scripts\Initialize-Toolchain.ps1 from $Root, then retry."
            }
            $rustVersion = (& $rustcExe --version).Trim()
            if ($LASTEXITCODE -ne 0 -or $rustVersion -notmatch '^rustc 1\.99\.1 ') { throw "Expected the pinned Rust 1.99.1 toolchain; found '$rustVersion'." }

            $node = Get-Command node.exe -ErrorAction SilentlyContinue
            $npm = Get-Command npm.cmd -ErrorAction SilentlyContinue
            if (-not $node -or -not $npm) { throw "Node.js 24.19.0 and npm 11.17.0 are required to rebuild. Install the pinned versions, then retry." }
            $nodeVersion = (& $node.Source --version).Trim()
            $npmVersion = (& $npm.Source --version).Trim()
            if ($LASTEXITCODE -ne 0 -or $nodeVersion -ne "v24.19.0" -or $npmVersion -ne "11.17.0") {
                throw "Expected Node.js v24.19.0 and npm 11.17.0; found node=$nodeVersion npm=$npmVersion."
            }
            Import-MsvcEnvironment

            $webviewRuntime = Get-ChildItem -LiteralPath $webviewRoot -Directory -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
                Sort-Object { [version]$_.Name } -Descending |
                Select-Object -First 1
            if (-not $webviewRuntime) { throw "WebView2 Runtime was not found. Install the Evergreen WebView2 Runtime, then retry." }

            $null = New-Item -ItemType Directory -Path $runnerRoot -Force
            $logName = "build-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')).log"
            $RunnerLog = Join-Path $runnerRoot $logName
            if (-not (Test-Path -LiteralPath (Join-Path $Root "node_modules\@tauri-apps\cli"))) {
                Write-Host "Installing the pinned frontend dependencies with npm ci."
                & $npm.Source ci --no-audit --no-fund 2>&1 | Tee-Object -FilePath $RunnerLog
                $npmExit = $LASTEXITCODE
                if ($npmExit -ne 0) { throw "npm ci failed with exit code $npmExit. Build log: $RunnerLog" }
            }

            $previousCargoTarget = $env:CARGO_TARGET_DIR
            try {
                $env:CARGO_TARGET_DIR = $targetRoot
                Write-Host "Building the desktop app. Build output is also recorded at $RunnerLog."
                & $npm.Source run tauri -- build --no-bundle 2>&1 | Tee-Object -FilePath $RunnerLog -Append
                $buildExit = $LASTEXITCODE
            } finally {
                if ($null -eq $previousCargoTarget) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
                else { $env:CARGO_TARGET_DIR = $previousCargoTarget }
            }
            if ($buildExit -ne 0) { throw "Tauri build failed with exit code $buildExit. Build log: $RunnerLog" }
            if (-not (Test-Path -LiteralPath $cachedExe -PathType Leaf)) { throw "Build reported success but executable is missing: $cachedExe. Build log: $RunnerLog" }

            $executableHash = (Get-FileHash -LiteralPath $cachedExe -Algorithm SHA256).Hash
            [ordered]@{
                schema_version = 1
                commit_sha = $head
                tree_sha = $tree
                source_fingerprint = $sourceFingerprint
                executable_sha256 = $executableHash
                built_utc = [DateTime]::UtcNow.ToString("o")
                build_log = $RunnerLog
            } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $cacheManifestPath -Encoding utf8
        }
    } elseif ($accepted) {
        Write-Host "Using acceptance build for exact clean commit $head."
        Write-Host "Acceptance evidence: $($accepted.Report)"
    }

    if ($WhatIfPreference) {
        Write-Host "What if: Launch $exe"
        return
    }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "The desktop executable is not available: $exe" }
    $exeHash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
    Write-Host "Source commit: $head"
    Write-Host "WebView2 Runtime: $($webviewRuntime.Name)"
    Write-Host "Executable SHA-256: $exeHash"
    if ($accepted) { Write-Host "Build source: exact-commit acceptance artifact" }
    else { Write-Host "Build source: current repository files; build evidence: $RunnerLog" }

    if ($PSCmdlet.ShouldProcess($exe, "Launch MAX Research Ground")) {
        $process = Start-Process -FilePath $exe -WorkingDirectory $Root -PassThru
        Start-Sleep -Milliseconds 800
        $process.Refresh()
        if ($process.HasExited) { throw "MAX Research Ground exited during startup (exit code $($process.ExitCode)). See the acceptance report or build log." }
        Write-Host "MAX Research Ground launched (PID $($process.Id))."
    }
} catch {
    Write-Host "ERROR: $($_.Exception.Message)" -ForegroundColor Red
    if ($RunnerLog) { Write-Host "Build log: $RunnerLog" }
    if (-not $WhatIfPreference -and $Host.Name -eq "ConsoleHost") { Read-Host "Press Enter to close this window" | Out-Null }
    exit 1
}
