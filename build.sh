#!/usr/bin/env bash
set -euo pipefail

REPO="syedinsaf/otaripper"
ZIP_URL="https://github.com/${REPO}/archive/refs/heads/main.zip"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
BASE_DIR="${SCRIPT_DIR:-$(pwd)}"
WORKDIR="$BASE_DIR/otaripper-native-build"
OUTDIR="$BASE_DIR/otaripper-native"

# Preflight: Rust / Cargo
if ! command -v cargo >/dev/null 2>&1; then
  echo "❌ Rust/Cargo not found."
  echo
  if [ -t 0 ]; then
    read -rp "➡️  Do you want to install Rust using rustup? [y/N]: " yn
    case "$yn" in
      [Yy]*)
        echo "📦 Installing Rust (rustup)..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck disable=SC1090
        source "$HOME/.cargo/env"
        ;;
      *)
        echo "❌ Rust not installed. Aborting."
        exit 1
        ;;
    esac
  else
    echo "❌ Non-interactive shell detected and Rust is not installed. Aborting."
    exit 1
  fi
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

# MSRV check
if [ -f "Cargo.toml" ]; then
  REQUIRED_RUST=$(grep '^rust-version' Cargo.toml | head -n1 | cut -d'=' -f2 | tr -d ' "''' | cut -d'-' -f1)
  if [ -n "$REQUIRED_RUST" ] && command -v rustc >/dev/null 2>&1; then
    CURRENT_RUST=$(rustc --version | awk '{print $2}')
    CURRENT_RUST_CLEAN=$(echo "$CURRENT_RUST" | cut -d'-' -f1)
    LOWEST_VERSION=$(printf '%s\n%s\n' "$REQUIRED_RUST" "$CURRENT_RUST_CLEAN" | sort -V | head -n1)
    if [ "$LOWEST_VERSION" != "$REQUIRED_RUST" ] && [ "$CURRENT_RUST_CLEAN" != "$REQUIRED_RUST" ]; then
      echo "❌ Rust version mismatch"
      echo "  Installed Rust : $CURRENT_RUST"
      echo "  Required Rust  : $REQUIRED_RUST"
      echo
      echo "Please run:"
      echo "  rustup update stable"
      exit 1
    fi
  fi
fi

echo "⚙️  Building Full version (release, CPU=native)..."
export RUSTFLAGS="-C target-cpu=native"
cargo build --release --locked
cp target/release/otaripper "$OUTDIR/otaripper"
chmod +x "$OUTDIR/otaripper"

# Cleanup
echo "🧹 Cleaning up..."

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
