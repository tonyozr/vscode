<#
.SYNOPSIS
    Build the `code` CLI binary and the standalone Claude proxy JS bundle.

.DESCRIPTION
    1. Compiles the Rust CLI (cli/ crate, ~10s incremental).
    2. Bundles the TypeScript proxy into a single portable JS file
       (out/vs/platform/agentHost/node/claudeProxyBundle.js, ~15ms via esbuild).

    The bundle is self-contained — it embeds all VS Code module dependencies and
    only externalises native addons (@vscode/deviceid, @vscode/windows-registry).
    It does NOT require a full `npm run transpile-client` or the whole `out/` tree.

    OpenSSL is baked in so you don't have to set it by hand every time.

    One-time prereqs:
        winget install ShiningLight.OpenSSL.Dev
        npm ci --ignore-scripts     # installs node_modules (skips native-addon gyp)
        cd build; npm ci; cd ..     # installs build-time tools incl. esbuild

.PARAMETER Release
    Build in release mode (optimised, smaller binary, ~30-60s). Default: debug (~10s).

.PARAMETER Run
    Optional bind target. If given, the freshly built binary is launched as
    `code agent proxy claude <Run>`. Examples: `-Run 8123` (TCP port) or
    `-Run \\.\pipe\claude` (named pipe).

.EXAMPLE
    ./build-code.ps1
    ./build-code.ps1 -Release
    ./build-code.ps1 -Release -Run 8123
#>
param([switch]$Release, [string]$Run)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent   # repo root (parent of cli/)

# cargo on PATH (rustup default location) if not already.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}

# OpenSSL dev (required to link russh/dev-tunnels). Install once with:
#   winget install ShiningLight.OpenSSL.Dev
$ossl = $env:OPENSSL_DIR
if (-not $ossl) { $ossl = "C:\Program Files\OpenSSL-Win64" }
if (Test-Path $ossl) {
    $env:OPENSSL_DIR         = $ossl
    $env:OPENSSL_INCLUDE_DIR = Join-Path $ossl "include"
    $env:OPENSSL_LIB_DIR     = Join-Path $ossl "lib\VC\x64\MT"
    $env:OPENSSL_STATIC      = "1"
} else {
    Write-Warning "OpenSSL not found at $ossl. Install: winget install ShiningLight.OpenSSL.Dev (or set `$env:OPENSSL_DIR)."
}

# --- 1. Rust CLI ---------------------------------------------------------------
$cargoArgs = @('build', '--bin', 'code')
if ($Release) { $cargoArgs += '--release' }
$profile = if ($Release) { 'release' } else { 'debug' }

Push-Location $PSScriptRoot
try {
    cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}
$exe = Join-Path $PSScriptRoot "target\$profile\code.exe"
Write-Host "Built: $exe" -ForegroundColor Green

# --- 2. JS bundle --------------------------------------------------------------
$esbuild  = Join-Path $root "build\node_modules\.bin\esbuild.cmd"
$entry    = Join-Path $root "src\vs\platform\agentHost\node\claudeProxyMain.ts"
$outfile  = Join-Path $root "out\vs\platform\agentHost\node\claudeProxyBundle.js"

if (-not (Test-Path $esbuild)) {
    Write-Warning "esbuild not found at $esbuild. Run: cd build; npm ci"
} else {
    & $esbuild $entry `
        --bundle `
        --platform=node `
        --format=esm `
        "--outfile=$outfile" `
        --external:@vscode/deviceid `
        "--external:@vscode/windows-registry"
    if ($LASTEXITCODE -ne 0) { throw "esbuild failed ($LASTEXITCODE)" }
    Write-Host "Bundle: $outfile" -ForegroundColor Green
}

# --- 3. dist/ ------------------------------------------------------------------
# Copy binary + bundle into dist/ so `code[.exe]` finds `claudeProxyBundle.js`
# next to itself without any env vars.
$dist = Join-Path $PSScriptRoot "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Copy-Item -Force $exe         (Join-Path $dist "code.exe")
if (Test-Path $outfile) {
    Copy-Item -Force $outfile (Join-Path $dist "claudeProxyBundle.js")
}
Write-Host "dist/: $dist" -ForegroundColor Green

# --- 4. Optional run -----------------------------------------------------------
if ($Run) {
    $dist_exe = Join-Path $PSScriptRoot "dist\code.exe"
    Write-Host "Running: code agent proxy claude $Run" -ForegroundColor Cyan
    & $dist_exe agent proxy claude $Run
}
