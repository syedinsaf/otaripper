$ErrorActionPreference = "Stop"

$REPO    = "syedinsaf/otaripper"
$ZIP_URL = "https://github.com/$REPO/archive/refs/heads/main.zip"

$BASE_DIR = [Environment]::GetFolderPath("MyDocuments")
$WORKDIR  = Join-Path $BASE_DIR "otaripper-native-build"
$OUTDIR   = Join-Path $BASE_DIR "otaripper-native"

# ---------------------------
# Preflight: Rust / Cargo
# ---------------------------
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "❌ Rust/Cargo not found."
    Write-Host

    $yn = Read-Host "➡️  Do you want to install Rust using rustup? [y/N]"
    if ($yn -match '^[Yy]') {
        Write-Host "📦 Installing Rust (rustup)..."

        Invoke-WebRequest https://win.rustup.rs -OutFile "rustup-init.exe"
        Start-Process -FilePath ".\rustup-init.exe" -ArgumentList "-y" -Wait

        # Make Cargo available in this session
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    }
    else {
        Write-Host "❌ Rust not installed. Aborting."
        exit 1
    }
}

# ---------------------------
# Build
# ---------------------------
Write-Host "⬇️  Downloading otaripper source..."

Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $WORKDIR, $OUTDIR
New-Item -ItemType Directory -Force -Path $WORKDIR, $OUTDIR | Out-Null
Set-Location $WORKDIR

Invoke-WebRequest -Uri $ZIP_URL -OutFile "otaripper.zip"

Write-Host "📦 Extracting..."
Expand-Archive -Path "otaripper.zip" -DestinationPath .

$SRC_DIR = Get-ChildItem -Directory |
    Where-Object { $_.Name -like "otaripper-*" } |
    Select-Object -First 1

Set-Location $SRC_DIR.FullName

Write-Host "⚙️  Building (release, CPU=native)..."
$env:RUSTFLAGS = "-C target-cpu=native"

cargo build --release

# ---------------------------
# Cleanup
# ---------------------------
Write-Host "🧹 Cleaning up..."
Copy-Item "target\release\otaripper.exe" $OUTDIR -Force

Set-Location $BASE_DIR
Remove-Item -Recurse -Force $WORKDIR

Write-Host ""
Write-Host "✅ Build complete"
Write-Host "📦 Binary location:"
Write-Host "  $OUTDIR\otaripper.exe"
Write-Host ""
Write-Host "⚠️  NOTE:"
Write-Host "This binary is optimized for *this* CPU only."
Write-Host "Do NOT redistribute it."
