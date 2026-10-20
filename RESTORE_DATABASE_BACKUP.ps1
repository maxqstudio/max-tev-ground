[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "High")]
param(
    # The research database. Defaults to the desktop application's store.
    [string]$DatabasePath = (Join-Path $env:APPDATA "com.max.research-ground\research.sqlite3"),
    # A backup listed by running this script without -Backup. Omit it to only list backups.
    [string]$Backup
)

# Lists or restores the verified backups the application writes before a database schema upgrade,
# for example to reopen the store with a previously accepted build. Close MAX Research Ground first.
# The current database files are renamed aside, never deleted.

$ErrorActionPreference = "Stop"
$Root = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd("\")
if ($Root -ne "D:\MAX_RESEARCH_GROUND") { throw "This script is bound to D:\MAX_RESEARCH_GROUND; found $Root" }
. (Join-Path $Root "scripts\Desktop-LauncherFunctions.ps1")
$env:CARGO_HOME = Join-Path $Root ".tools\cargo"
$env:RUSTUP_HOME = Join-Path $Root ".tools\rustup"
$env:PATH = "$(Join-Path $env:CARGO_HOME 'bin');$env:PATH"
$cargo = Join-Path $env:CARGO_HOME "bin\cargo.exe"
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
    throw "The project-local Rust toolchain is missing. Run .\scripts\Initialize-Toolchain.ps1 first."
}
if (-not (Test-Path -LiteralPath $DatabasePath -PathType Leaf)) { throw "Database not found: $DatabasePath" }

$log = Join-Path ([System.IO.Path]::GetTempPath()) ("rg-schema-backup-" + [Guid]::NewGuid().ToString("N") + ".log")
Push-Location -LiteralPath $Root
try {
    if (-not $Backup) {
        $exit = Invoke-LoggedNativeCommand -LogPath $log -Command {
            & $cargo run --quiet --locked -p rg-persistence --bin rg-schema-backup -- list $DatabasePath 2>&1
        }
        if ($exit -ne 0) { throw "Listing backups failed with exit code $exit. Log: $log" }
        return
    }
    if (Get-Process -Name "max-research-ground-desktop" -ErrorAction SilentlyContinue) {
        throw "MAX Research Ground is running. Close it before restoring a database backup."
    }
    if ($PSCmdlet.ShouldProcess($DatabasePath, "Restore schema backup $Backup (current files are kept beside it)")) {
        $exit = Invoke-LoggedNativeCommand -LogPath $log -Command {
            & $cargo run --quiet --locked -p rg-persistence --bin rg-schema-backup -- restore $DatabasePath $Backup 2>&1
        }
        if ($exit -ne 0) { throw "Restore failed with exit code $exit. The log names any files that were moved aside: $log" }
    }
} finally {
    Pop-Location
}
