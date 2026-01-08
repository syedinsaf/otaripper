<!-- markdownlint-configure-file {
  "MD033": false,
  "MD041": false
} -->

<div align="center">

# otaripper

**Fast, safe, and reliable Android OTA partition extractor**

Extract partitions from Android OTA files with cryptographic verification, strong reliability guarantees, and high-performance execution.

[![GitHub release](https://img.shields.io/github/v/release/syedinsaf/otaripper?style=for-the-badge)](https://github.com/syedinsaf/otaripper/releases)
[![Downloads](https://img.shields.io/github/downloads/syedinsaf/otaripper/total?style=for-the-badge)](https://github.com/syedinsaf/otaripper/releases)
[![License](https://img.shields.io/github/license/syedinsaf/otaripper?style=for-the-badge)](LICENSE)

[Download](https://github.com/syedinsaf/otaripper/releases) • [Quick Start](#quick-start) • [Build Guide](#building-from-source) • [Technical Details](TECHNICAL.md)

</div>

---

## Table of Contents

* [Overview](#overview)
* [Feature Comparison](#feature-comparison)
* [Performance](#performance)
* [Quick Start](#quick-start)
  * [Installation](#installation)
  * [Build from Source](#build-from-source)
* [Basic Usage](#basic-usage)
* [Command Options](#command-options)
* [Usage Examples](#usage-examples)
* [Building from Source](#building-from-source)
* [Contributing](#contributing)
* [Acknowledgments](#acknowledgments)
* [License](#license)
* [Disclaimer](#disclaimer)

For detailed technical documentation, see [TECHNICAL.md](TECHNICAL.md)

---

## Overview

otaripper extracts partitions from Android OTA packages (`payload.bin` or full OTA `.zip` files). The tool is built in Rust and prioritizes:

* Correctness and data integrity
* Predictable and fail-safe behavior
* Multi-threaded and SIMD-accelerated performance
* Memory safety
* Portability and ease of use

Unlike most extraction tools, otaripper verifies output images to prevent corrupted partitions from being produced, reducing the risk of flashing invalid or damaged files.

Incremental OTA packages are intentionally not supported.

---

## Feature Comparison

|                         | otaripper v2.0        | payload-dumper-go | payload_dumper (Python) |
| ----------------------- | --------------------- | ----------------- | ----------------------- |
| Output verification     | SHA-256 verified      | No                | No                      |
| SIMD optimization       | AVX-512 / AVX2 / SSE2 | No                | No                      |
| Graceful interruption   | Yes                   | No                | No                      |
| Auto-cleanup on failure | Yes                   | No                | No                      |
| Performance statistics  | Yes                   | No                | No                      |
| Selective extraction    | Yes                   | Yes               | Yes                     |
| Direct ZIP support      | Yes                   | Yes               | No                      |
| Multi-threaded          | Yes                   | Yes               | No (single-threaded)    |
| Cross-platform          | Win/Linux/macOS       | Yes               | Requires Python         |
| Standalone binary       | Yes                   | Yes               | No                      |

> otaripper is designed to avoid producing corrupted or partially valid images. Failure handling is deliberate and conservative.

---

## Performance

otaripper automatically detects CPU capabilities and uses the most appropriate SIMD instruction set available. Performance scales with storage speed, CPU capability, and OTA compression format.

```
Throughput Example (3GB system partition)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
otaripper (AVX-512)  ████████████ 2.8 GB/s
otaripper (AVX2)     ████████     1.9 GB/s
payload-dumper-go    ████         1.0 GB/s
payload_dumper       ██           0.4 GB/s
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

Performance improves significantly on SSD storage and modern CPUs.

For detailed performance architecture, see [TECHNICAL.md](TECHNICAL.md#performance-architecture)

---

## Quick Start

### Installation

**Prebuilt binaries** are available in [Releases](https://github.com/syedinsaf/otaripper/releases):

* Windows: `otaripper-x86_64-pc-windows-msvc.exe`
* Linux (glibc): `otaripper-x86_64-unknown-linux-gnu`
* Linux (musl): `otaripper-x86_64-unknown-linux-musl`
* macOS (Intel): `otaripper-x86_64-apple-darwin`
* macOS (Apple Silicon): `otaripper-aarch64-apple-darwin`

### Build from Source

```bash
git clone https://github.com/syedinsaf/otaripper.git
cd otaripper
cargo build --release
```

Binary location:
* Linux/macOS: `target/release/otaripper`
* Windows: `target/release/otaripper.exe`

---

## Basic Usage

Extract everything:

```bash
otaripper ota.zip
```

List partitions:

```bash
otaripper -l ota.zip
```

Extract selected partitions:

```bash
otaripper ota.zip -p boot,vendor_boot,init_boot
```

Specify output directory:

```bash
otaripper ota.zip -o ~/extracted
```

Print hashes:

```bash
otaripper ota.zip --print-hash
```

Strict verification mode:

```bash
otaripper ota.zip --strict
```

Disable automatic folder opening:

```bash
otaripper ota.zip -n
```

Performance statistics:

```bash
otaripper ota.zip --stats
```

---

## Example Output

```
Extraction in progress. Use Ctrl+C to cancel safely.
Processing 12 partitions using 16 threads...

           boot [████████████████████] 100%
    vendor_boot [████████████████████] 100%
         system [████████████████████] 100%
      system_ext [████████████████████] 100%

Extraction statistics:
  - boot: 64.0 MB in 45 ms (1.42 GB/s)
  - vendor_boot: 128.0 MB in 67 ms (1.91 GB/s)
  - system: 2.1 GB in 1205 ms (1.74 GB/s)

Partition hashes (SHA-256):
boot: sha256=a1b2c3d4e5f67890...
vendor_boot: sha256=b2c3d4e5f67890a1...

Extraction completed successfully.
Output directory: /home/user/extracted_2024-12-25_14-30-22
```

---

## Command Options

| Option                  | Description                                      |
| ----------------------- | ------------------------------------------------ |
| `-l, --list`            | List partitions without extracting               |
| `-p, --partitions`      | Extract only specific partitions (comma-separated) |
| `-o, --output-dir PATH` | Custom output directory                          |
| `--strict`              | Require manifest hashes and enforce verification |
| `--no-verify`           | Disable all verification (not recommended)       |
| `--print-hash`          | Print SHA-256 hashes for extracted partitions    |
| `--sanity`              | Enable corruption detection checks               |
| `--stats`               | Show performance statistics                      |
| `-t, --threads N`       | Worker thread control (1–256, 0 for auto)        |
| `-n, --no-open`         | Disable automatic folder opening                 |

### Safety Options Explained

**`--strict`**: Maximum security mode
* Enforces cryptographic verification for all operations
* Fails immediately if any hash is missing from manifest
* Automatically cleans up on any verification failure
* Cannot be combined with `--no-verify`

**`--print-hash`**: Hash verification
* Displays SHA-256 hash for each extracted partition
* Reuses computed hashes (no extra passes)
* Format: `partition: sha256=abc123...`

**`--sanity`**: Corruption detection
* Detects obviously corrupted output (e.g., all-zero images)
* Lightweight checks with near-zero overhead
* Catches issues even when manifest hashes are missing

**`--no-verify`**: Fast but dangerous
* Skips all hash verification (faster but risky)
* Only for trusted sources or debugging
* Cannot be used with `--strict`

---

## Usage Examples

### For Beginners

**Extract boot image for rooting**
```bash
# Extract just the boot partition
./otaripper ota.zip -p boot

# Verify it's correct
./otaripper ota.zip -p boot --print-hash
```

**Check contents before extracting**
```bash
./otaripper -l ota.zip
```

### For ROM Developers

**Extract system partitions with verification**
```bash
./otaripper ota.zip \
  -p system,system_ext,product,vendor \
  --strict \
  --print-hash \
  --stats
```

**Performance benchmarking**
```bash
for threads in 1 4 8 16 32; do
  echo "Testing with $threads threads:"
  ./otaripper ota.zip -t $threads --stats --no-open
done
```

### For Custom Recovery Developers

**Extract recovery-related partitions**
```bash
./otaripper ota.zip \
  -p boot,recovery,vendor_boot,dtbo,vbmeta \
  --print-hash \
  -o ~/recovery-dev
```

### For Security Researchers

**Forensic extraction with full verification**
```bash
# Maximum security + integrity checks
./otaripper ota.zip \
  --strict \
  --sanity \
  --print-hash \
  --stats \
  -o /secure/evidence

# Save output for chain of custody
./otaripper ota.zip --print-hash > sha256sums.txt
```

---

## Building from Source

### Prerequisites

* Rust 1.92.0+ ([Install rustup](https://rustup.rs/))
* Git
* C compiler (gcc/clang/MSVC)

### Build Commands

```bash
# Clone repository
git clone https://github.com/syedinsaf/otaripper.git
cd otaripper

# Build release binary
cargo build --release

# Run tests
cargo test --release

# Generate documentation
cargo doc --open
```

### Platform-Specific Builds

**Linux (glibc)**
```bash
cargo build --release --target x86_64-unknown-linux-gnu
```

**Linux (musl) - Static Binary**
```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

**Windows (MSVC)**
```bash
cargo build --release --target x86_64-pc-windows-msvc
```

**Windows (MinGW)**
```bash
cargo build --release --target x86_64-pc-windows-gnu
```

**macOS**
```bash
# Intel
cargo build --release --target x86_64-apple-darwin

# Apple Silicon
cargo build --release --target aarch64-apple-darwin
```

---

## Contributing

Contributions, bug reports, and testing feedback are welcome.

Please include:
* System details (OS, CPU, RAM)
* otaripper version
* Logs where possible
* Steps to reproduce

Pull requests are encouraged. Please:
1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Run tests (`cargo test`)
5. Format code (`cargo fmt`)
6. Commit with clear messages
7. Push and create a Pull Request

---

## Acknowledgments

This project benefits significantly from community testing and feedback.

Special thanks to **[Jean Rivera](https://github.com/jeanrivera)** for extensive real-world testing, validation work, and constructive feedback that helped improve correctness and robustness.

Thanks also to:
* Android Open Source Project documentation
* Rust ecosystem maintainers
* Users who reported issues and edge cases
* All contributors who help make otaripper better

---

## License

otaripper is licensed under the **Apache License 2.0**. See [LICENSE](LICENSE) for details.

---

## Show Your Support

If otaripper helped you:

* **Star the repository** — https://github.com/syedinsaf/otaripper
* **Report issues** — https://github.com/syedinsaf/otaripper/issues
* **Submit pull requests** — https://github.com/syedinsaf/otaripper/pulls
* **Share with others** — https://github.com/syedinsaf/otaripper


---

<div align="left">

**Made by [Syed Insaf](https://github.com/syedinsaf)**

**Fast • Safe • Reliable**

</div>

---

## Disclaimer

Use at your own risk.

* Always verify extracted partitions before flashing
* Keep backups where possible
* Understand your device and bootloader requirements

The author is not responsible for data loss, bricked devices, or damage resulting from misuse.

By using this tool, you acknowledge these risks and agree that you will not hold the author or maintainers liable for any damages or losses.
