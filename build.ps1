$ErrorActionPreference = "Stop"

$REPO = "syedinsaf/otaripper"
$ZIP_URL = "https://github.com/$REPO/archive/refs/heads/main.zip"

$BASE_DIR = $PSScriptRoot
$WORKDIR = Join-Path $BASE_DIR "otaripper-native-build"
$OUTDIR = Join-Path $BASE_DIR "otaripper-native"

# Preflight: Rust / Cargo
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "ERROR: Rust/Cargo not found."
    Write-Host ""

    $yn = Read-Host "Do you want to install Rust using rustup? [y/N]"
    if ($yn -match '^[Yy]') {
        Write-Host "Installing Rust (rustup)..."

        Invoke-WebRequest https://win.rustup.rs -OutFile "rustup-init.exe"
        Start-Process -FilePath ".\rustup-init.exe" -ArgumentList "-y" -Wait

        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
        rustup update stable
    }
    else {
        Write-Host "ERROR: Rust not installed. Aborting."
        exit 1
    }
}

# Download source
Write-Host "Downloading otaripper source..."

Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $WORKDIR, $OUTDIR
New-Item -ItemType Directory -Force -Path $WORKDIR, $OUTDIR | Out-Null
Set-Location $WORKDIR

Invoke-WebRequest -Uri $ZIP_URL -OutFile "otaripper.zip"
Write-Host "Extracting..."
Expand-Archive -Path "otaripper.zip" -DestinationPath .

$SRC_DIR = Get-ChildItem -Directory |
Where-Object { $_.Name -like "otaripper-*" } |
Select-Object -First 1

Set-Location $SRC_DIR.FullName

# MSRV check (from Cargo.toml)
$cargoToml = Get-Content "Cargo.toml"

$msrvLine = $cargoToml | Where-Object { $_ -match '^rust-version\s*=' }
if (-not $msrvLine) {
    Write-Host "WARNING: No rust-version field found in Cargo.toml."
}
else {
    $requiredRust = ($msrvLine -split '=')[1].Trim().Trim('"')
    $currentRust = (rustc --version).Split()[1]

    function Format-Version($v) {
        return [version]($v -replace "-.*$", "")
    }

    if ((Format-Version $currentRust) -lt (Format-Version $requiredRust)) {
        Write-Host ""
        Write-Host "ERROR: Rust version mismatch"
        Write-Host "  Installed Rust : $currentRust"
        Write-Host "  Required Rust  : $requiredRust"
        Write-Host ""
        Write-Host "Please run:"
        Write-Host "  rustup update stable"
        Write-Host ""
        exit 1
    }
}

# Reproducible build
Write-Host "Fetching dependencies (locked)..."
cargo fetch --locked

Write-Host "Building Full version (release, CPU=native)..."

$oldRustFlags = $env:RUSTFLAGS
$env:RUSTFLAGS = "-C target-cpu=native"

cargo build --release --locked
$BIN = "target\release\otaripper.exe"
if (-not (Test-Path $BIN)) {
    Write-Host "ERROR: Full build failed."
    exit 1
}
Copy-Item $BIN "$OUTDIR\otaripper.exe" -Force

$env:RUSTFLAGS = $oldRustFlags

# Cleanup
Write-Host "Finalizing..."

Set-Location $BASE_DIR
Remove-Item -Recurse -Force $WORKDIR

Write-Host ""
Write-Host "Build complete."
Write-Host "Binary location:"
Write-Host "  $OUTDIR\otaripper.exe"
Write-Host ""
Write-Host "NOTE:"
Write-Host "This binary is optimized for THIS CPU only."
Write-Host "Do NOT redistribute it."
