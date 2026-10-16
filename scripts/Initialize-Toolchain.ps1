$ErrorActionPreference = "Stop"
$Root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..")).TrimEnd("\")
if ($Root -ne "D:\MAX_RESEARCH_GROUND") {
    throw "This project-local bootstrap is bound to D:\MAX_RESEARCH_GROUND; found $Root"
}

$toolchainText = Get-Content -LiteralPath (Join-Path $Root "rust-toolchain.toml") -Raw
$channelMatch = [regex]::Match($toolchainText, '(?m)^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"')
if (-not $channelMatch.Success) {
    throw "Could not read the pinned Rust channel from rust-toolchain.toml"
}
$RustVersion = $channelMatch.Groups[1].Value
$RustupVersion = "1.29.1"
$Target = "x86_64-pc-windows-msvc"
$env:RUSTUP_HOME = Join-Path $Root ".tools\rustup"
$env:CARGO_HOME = Join-Path $Root ".tools\cargo"
$env:PATH = "$env:CARGO_HOME\bin;$env:PATH"

$bootstrapDir = Join-Path $Root "work\bootstrap"
New-Item -ItemType Directory -Path $bootstrapDir -Force | Out-Null
$rustup = Join-Path $env:CARGO_HOME "bin\rustup.exe"
if (-not (Test-Path -LiteralPath $rustup)) {
    $installer = Join-Path $bootstrapDir "rustup-init-$RustupVersion.exe"
    $url = "https://static.rust-lang.org/rustup/archive/$RustupVersion/$Target/rustup-init.exe"
    $checksumUrl = "$url.sha256"
    Invoke-WebRequest -Uri $url -OutFile $installer
    $checksumBytes = (Invoke-WebRequest -Uri $checksumUrl).Content
    $expected = ([System.Text.Encoding]::UTF8.GetString($checksumBytes) -split '\s+')[0].ToUpperInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer).Hash.ToUpperInvariant()
    if ($expected -ne $actual) {
        throw "rustup-init SHA-256 verification failed"
    }
    & $installer -y --no-modify-path --default-host $Target --default-toolchain $RustVersion --profile minimal
    if ($LASTEXITCODE -ne 0) {
        throw "rustup-init exited with $LASTEXITCODE"
    }
}

& $rustup toolchain install $RustVersion --profile minimal --target $Target --component clippy --component rustfmt
if ($LASTEXITCODE -ne 0) {
    throw "Pinned Rust toolchain installation failed with $LASTEXITCODE"
}
& $rustup component add clippy rustfmt --toolchain "$RustVersion-$Target"
if ($LASTEXITCODE -ne 0) {
    throw "Pinned Rust components installation failed with $LASTEXITCODE"
}
& (Join-Path $env:CARGO_HOME "bin\rustc.exe") --version
& (Join-Path $env:CARGO_HOME "bin\cargo.exe") --version
Write-Output "PROJECT_LOCAL_TOOLCHAIN_READY=$RustVersion-$Target"