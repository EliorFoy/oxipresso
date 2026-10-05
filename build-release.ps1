# Build script for the shipped Oxipresso CLI binary.
#
# Usage:
#   .\build-release.ps1                  # stub engine (no TeX dependencies)
#   .\build-release.ps1 -RealEngine      # link the real TeXpresso/XeTeX engine
#   .\build-release.ps1 -RealEngine -Gui # also build the -gui live preview window
#
# The produced binary lands in target\release\oxipresso.exe (plus
# oxipresso-editor-client.exe when -Slint is passed).
param(
    [switch]$RealEngine,
    [switch]$Gui,
    [switch]$Slint,
    [switch]$Freetype,
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"

# --- environment for the real engine -----------------------------------------
if ($RealEngine) {
    $env:OXIPRESSO_USE_REAL_XETEX = "1"
    if (-not $env:TEXPRESSO_SRC) {
        $candidate = "F:\code\texpresso-src"
        if (Test-Path $candidate) { $env:TEXPRESSO_SRC = $candidate }
        else {
            Write-Error "RealEngine needs TEXPRESSO_SRC (the texpresso C/C++ source tree); set it or accept the default $candidate"
            exit 1
        }
    }
    if (-not $env:VCPKG_ROOT) {
        $candidate = "F:\code\vcpkg"
        if (Test-Path $candidate) { $env:VCPKG_ROOT = $candidate }
        else {
            Write-Error "RealEngine needs VCPKG_ROOT (freetype/harfbuzz/icu/... triplets); set it or accept the default $candidate"
            exit 1
        }
    }
    Write-Host "real engine: TEXPRESSO_SRC=$env:TEXPRESSO_SRC VCPKG_ROOT=$env:VCPKG_ROOT"
}

# --- features ----------------------------------------------------------------
$features = @()
if ($Gui) { $features += "gui" }
if ($Slint) { $features += "slint" }
if ($Freetype) { $features += "freetype" }
$featureArgs = @()
if ($features.Count -gt 0) { $featureArgs = @("--features", ($features -join ",")) }

# --- tests -------------------------------------------------------------------
if (-not $SkipTests) {
    Write-Host "== workspace tests (stub contract) =="
    cargo test --workspace
    if ($LASTEXITCODE -ne 0) { Write-Error "workspace tests failed"; exit 1 }
}

# --- build -------------------------------------------------------------------
Write-Host "== release build =="
cargo build --release -p oxipresso-cli @featureArgs
if ($LASTEXITCODE -ne 0) { Write-Error "release build failed"; exit 1 }

$exe = Join-Path (Get-Location) "target\release\oxipresso.exe"
if (-not (Test-Path $exe)) { Write-Error "expected binary missing: $exe"; exit 1 }
Write-Host ("built: {0} ({1:N0} KB)" -f $exe, ((Get-Item $exe).Length / 1KB))

# --- smoke: the shipped binary answers the wire ------------------------------
Write-Host "== wire smoke (stub path answers without a TeX distribution) =="
if (-not $RealEngine) {
    $doc = Join-Path $env:TEMP ("oxi-smoke-{0}.tex" -f [guid]::NewGuid().ToString("N"))
    [IO.File]::WriteAllText($doc, "\documentclass{article}`n\begin{document}`nsmoke`n\end{document}`n")
    $out = & $exe -test-initialize $doc 2>$null
    if ($LASTEXITCODE -ne 0) { Write-Error "smoke run failed with exit code $LASTEXITCODE"; exit 1 }
    Remove-Item $doc -ErrorAction SilentlyContinue
    Write-Host "smoke: -test-initialize exited 0"
} else {
    Write-Host "smoke: skipped for RealEngine (needs a TeX distribution + format file; run the binary with OXIPRESSO_XETEX_FORMAT set)"
}

Write-Host ""
Write-Host "next steps:"
Write-Host "  oxipresso.exe -gui document.tex        # live preview window"
Write-Host "  set OXIPRESSO_RESIDENT=1               # checkpoint hot rebuilds (real engine + prebuilt format)"
