# XEarthLayer local verification loop (Windows).
#
# Mirrors the Windows-relevant targets of `make verify` — format-check, lint,
# test-strict — so the same checks CI runs can be executed locally without
# installing GNU make. `test-scripts` is deliberately excluded: it is the
# bash-based test suite for the macOS release tooling, and CI covers it.
#
# Usage (from any directory):
#   powershell -ExecutionPolicy Bypass -File scripts\verify.ps1
#   pwsh scripts/verify.ps1          # if PowerShell 7 is available

Set-Location (Split-Path -Parent $PSScriptRoot)

# Match CI (`env: RUSTFLAGS: -Dwarnings`) and the Makefile's strict targets:
# warnings are errors for both clippy's compilation and the test run.
$env:RUSTFLAGS = "-Dwarnings"
$env:RUST_BACKTRACE = "1"

function Invoke-Step {
    param([string]$Name, [scriptblock]$Script)
    Write-Host ""
    Write-Host "==> $Name" -ForegroundColor Cyan
    & $Script
    if ($LASTEXITCODE -ne 0) {
        Write-Host ""
        Write-Host "FAILED: $Name (exit code $LASTEXITCODE)" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Invoke-Step "cargo fmt --check" { cargo fmt -- --check }
Invoke-Step "cargo clippy (-D warnings)" { cargo clippy -- -D warnings }
Invoke-Step "cargo test (strict)" { cargo test -- --nocapture }

Write-Host ""
Write-Host "All local verification checks passed." -ForegroundColor Green
