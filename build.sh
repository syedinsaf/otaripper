#!/bin/bash

# Build script for otaripper with platform-specific optimizations

echo "Building otaripper..."

# Detect the platform
if [[ "$OSTYPE" == "linux-gnu"* ]] || [[ "$OSTYPE" == "darwin"* ]]; then
    echo "Detected Unix-like system (Linux/macOS)"
    echo "Building with maximum performance optimizations..."
    cargo build --release --features "asm-sha2,fast-compression,high-performance"
elif [[ "$OSTYPE" == "msys" ]] || [[ "$OSTYPE" == "cygwin" ]]; then
    echo "Detected Windows (MSYS/Cygwin)"
    echo "Building with Windows-optimized features..."
    cargo build --release --features "asm-sha2,fast-compression,high-performance,windows-optimized"
else
    echo "Unknown platform, building with default optimizations..."
    cargo build --release --features "asm-sha2,fast-compression"
fi

echo "Build completed!"
