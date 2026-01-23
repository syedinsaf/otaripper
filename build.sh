#!/usr/bin/env bash
set -euo pipefail

REPO="syedinsaf/otaripper"
ZIP_URL="https://github.com/${REPO}/archive/refs/heads/main.zip"

BASE_DIR="$HOME/Documents"
WORKDIR="$BASE_DIR/otaripper-native-build"
OUTDIR="$BASE_DIR/otaripper-native"

# Preflight: Rust / Cargo
if ! command -v cargo >/dev/null 2>&1; then
  echo "❌ Rust/Cargo not found."
  echo
  read -rp "➡️  Do you want to install Rust using rustup? [y/N]: " yn
  case "$yn" in
    [Yy]*)
      echo "📦 Installing Rust (rustup)..."
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
      # shellcheck disable=SC1090
      source "$HOME/.cargo/env"
      ;;
    *)
      echo "❌ Rust not installed. Aborting."
      exit 1
      ;;
  esac
fi

# Build
echo "⬇️  Downloading otaripper source..."

rm -rf "$WORKDIR" "$OUTDIR"
mkdir -p "$WORKDIR" "$OUTDIR"
cd "$WORKDIR"

curl -L "$ZIP_URL" -o otaripper.zip

echo "📦 Extracting..."
unzip -q otaripper.zip
cd otaripper-*

echo "⚙️  Building (release, CPU=native)..."
export RUSTFLAGS="-C target-cpu=native"
cargo build --release

# Cleanup
echo "🧹 Cleaning up..."
cp target/release/otaripper "$OUTDIR/"

cd "$BASE_DIR"
rm -rf "$WORKDIR"

echo
echo "✅ Build complete"
echo "📦 Binary location:"
echo "  $OUTDIR/otaripper"
echo
echo "⚠️  NOTE:"
echo "This binary is optimized for *this* CPU only."
echo "Do NOT redistribute it."
