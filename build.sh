#!/bin/bash

# Build script for otaripper with platform-specific optimizations

echo "Building otaripper..."

# Detect the platform
if [[ "$OSTYPE" == "linux-gnu"* ]] || [[ "$OSTYPE" == "darwin"* ]]; then
    echo "Detected Unix-like system (Linux/macOS)"
    echo "Building with assembly optimizations for SHA2..."
    cargo build --release --features asm-sha2
elif [[ "$OSTYPE" == "msys" ]] || [[ "$OSTYPE" == "cygwin" ]]; then
    echo "Detected Windows (MSYS/Cygwin)"
    echo "Building without assembly optimizations for compatibility..."
    cargo build --release
else
    echo "Unknown platform, building with default features..."
    cargo build --release
fi

echo "Build completed!"
