# Rust Master Workflow Verification Script (Windows)

Write-Host "--- Starting Rust Master Workflow Verification ---" -ForegroundColor Cyan

# 1. Format Check
Write-Host "[1/3] Checking format..." -ForegroundColor Yellow
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) {
    Write-Host "Format check failed. Run 'cargo fmt' to fix." -ForegroundColor Red
    exit 1
}

# 2. Clippy (Zero Warnings Gate)
Write-Host "[2/3] Running Clippy..." -ForegroundColor Yellow
cargo clippy -- -D warnings
if ($LASTEXITCODE -ne 0) {
    Write-Host "Clippy check failed. Please fix all warnings." -ForegroundColor Red
    exit 1
}

# 3. Tests
Write-Host "[3/3] Running tests..." -ForegroundColor Yellow
cargo test
if ($LASTEXITCODE -ne 0) {
    Write-Host "Tests failed." -ForegroundColor Red
    exit 1
}

Write-Host "--- Verification Passed! ---" -ForegroundColor Green
