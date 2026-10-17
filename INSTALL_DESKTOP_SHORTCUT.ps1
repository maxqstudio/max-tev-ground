[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "Low")]
param()

$ErrorActionPreference = "Stop"
$ExpectedRoot = "D:\MAX_RESEARCH_GROUND"
$Root = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd("\")
if ($Root -ne $ExpectedRoot) { throw "Shortcut installer is bound to $ExpectedRoot; found $Root." }

$desktop = [Environment]::GetFolderPath("Desktop")
if ([string]::IsNullOrWhiteSpace($desktop) -or -not (Test-Path -LiteralPath $desktop -PathType Container)) {
    throw "The current user's Desktop folder could not be resolved."
}
$shortcutPath = Join-Path $desktop "MAX Research Ground.lnk"
$runnerPath = Join-Path $Root "RUN_DESKTOP.ps1"
$powerShellPath = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
if (-not (Test-Path -LiteralPath $runnerPath -PathType Leaf)) { throw "Desktop runner is missing: $runnerPath" }
if (-not (Test-Path -LiteralPath $powerShellPath -PathType Leaf)) { throw "Windows PowerShell is missing: $powerShellPath" }

if (Test-Path -LiteralPath $shortcutPath -PathType Leaf) {
    $shell = New-Object -ComObject WScript.Shell
    $existing = $shell.CreateShortcut($shortcutPath)
    $expectedArguments = "-NoLogo -NoProfile -ExecutionPolicy Bypass -File `"$runnerPath`""
    if ($existing.TargetPath -ine $powerShellPath -or $existing.Arguments -ine $expectedArguments) {
        throw "A different shortcut already exists at $shortcutPath; it was left unchanged."
    }
    Write-Host "MAX Research Ground shortcut is already installed: $shortcutPath"
    return
}

if ($PSCmdlet.ShouldProcess($shortcutPath, "Create a Desktop shortcut for MAX Research Ground")) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $powerShellPath
    $shortcut.Arguments = "-NoLogo -NoProfile -ExecutionPolicy Bypass -File `"$runnerPath`""
    $shortcut.WorkingDirectory = $Root
    $shortcut.Description = "Open MAX Research Ground"
    $shortcut.WindowStyle = 1

    $acceptanceRoot = Join-Path $Root "artifacts\acceptance"
    if (Test-Path -LiteralPath $acceptanceRoot -PathType Container) {
        $iconExe = Get-ChildItem -LiteralPath $acceptanceRoot -Filter "max-research-ground-desktop.exe" -File -Recurse -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTimeUtc -Descending |
            Select-Object -First 1
        if ($iconExe) { $shortcut.IconLocation = "$($iconExe.FullName),0" }
    }
    $shortcut.Save()
    Write-Host "Created Desktop shortcut: $shortcutPath"
    Write-Host "It runs $runnerPath and can rebuild a stale or missing desktop binary."
}
