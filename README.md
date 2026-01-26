<!-- markdownlint-configure-file {
  "MD033": false,
  "MD041": false
} -->

<div align="center">

# otaripper

Extract partitions from Android OTA files with cryptographic verification, strong reliability guarantees, and high-performance execution.

[![Crates.io](https://img.shields.io/crates/v/otaripper?style=for-the-badge&logo=rust&logoColor=white&label=crates.io&color=rust)](https://crates.io/crates/otaripper)
[![GitHub release](https://img.shields.io/github/v/release/syedinsaf/otaripper?style=for-the-badge&logo=github&logoColor=white&color=rust)](https://github.com/syedinsaf/otaripper releases)
[![Downloads](https://img.shields.io/github/downloads/syedinsaf/otaripper/total?style=for-the-badge&logo=github&logoColor=white&color=rust)](https://github.com/syedinsaf/otaripper/releases)
[![License](https://img.shields.io/github/license/syedinsaf/otaripper?style=for-the-badge&logo=github&logoColor=white&color=rust)](LICENSE)


[Download](https://github.com/syedinsaf/otaripper/releases) •
[Quick Start](#quick-start) •
[Build Guide](#building-from-source) •
[Technical Details](TECHNICAL.md)

</div>

---

## Table of Contents

* [Overview](#overview)
* [Feature Comparison](#feature-comparison)
* [Performance](#performance)
* [Quick Start](#quick-start)
* [Basic Usage](#basic-usage)
* [Cleanup](#cleanup)
* [Command Options](#command-options)
* [Building from Source](#building-from-source)
* [Contributing](#contributing)
* [Acknowledgments](#acknowledgments)
* [Show Your Support](#show-your-support)
* [License](#license)
* [Disclaimer](#disclaimer)

For in-depth architecture and performance details, see [TECHNICAL.md](TECHNICAL.md)

---

## Overview

**otaripper** extracts partitions from Android OTA packages (`payload.bin` or full OTA `.zip` files).

The tool is written in Rust and prioritizes:

* Cryptographic correctness and data integrity
* Predictable, fail-safe behavior
* High-performance, multi-threaded execution
* SIMD-accelerated memory operations
* Guaranteed cleanup on failure or interruption

Unlike many extraction tools, otaripper **verifies output images by default** and refuses to leave behind partially valid or corrupted files.

⚠️ Incremental OTA packages are intentionally **not supported**.

---

## Feature Comparison

|                         | otaripper v2.1 | payload-dumper-go | payload_dumper (Python) |
| ----------------------- | -------------- | ----------------- | ----------------------- |
| Output verification     | ✅ SHA-256      | ❌                | ❌                      |
| SIMD optimization       | ✅ AVX-512 / AVX2 / SSE2 | ❌        | ❌                      |
| Cache-aware large writes| ✅              | ❌                | ❌                      |
| Graceful interruption   | ✅              | ❌                | ❌                      |
| Auto-cleanup on failure | ✅              | ❌                | ❌                      |
| Performance statistics  | ✅              | ❌                | ❌                      |
| Selective extraction    | ✅              | ✅                | ✅                      |
| Direct ZIP support      | ✅              | ✅                | ❌                      |
| Multi-threaded          | ✅              | ✅                | ❌ (single-threaded)    |
| Cross-platform          | ✅              | ✅                | ⚠️ Requires Python     |
| Standalone binary       | ✅              | ✅                | ❌                      |

> otaripper is designed to fail early and cleanly rather than produce questionable output.

---

## Performance

otaripper automatically detects CPU capabilities and selects the optimal execution path.

Version **2.1** refines critical hot paths by:

* specializing single-extent writes
* reducing bounds checks in tight loops
* using cache-bypassing SIMD stores for large write-once buffers

```

Throughput Example (3GB system partition)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
otaripper (AVX-512)  ████████████ 2.8 GB/s
otaripper (AVX2)     ████████     1.9 GB/s
payload-dumper-go    ████         1.0 GB/s
payload_dumper       ██           0.4 GB/s
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

````

Performance scales with:

* storage speed (NVMe > SATA > HDD)
* compression format
* CPU SIMD capability

For architectural details, see
[TECHNICAL.md#performance-architecture](TECHNICAL.md#performance-architecture)

---

## Quick Start

### Installation

Prebuilt binaries are available on the
[Releases](https://github.com/syedinsaf/otaripper/releases) page:

* Windows: `otaripper-x86_64-pc-windows-msvc.exe`
* Linux (glibc): `otaripper-x86_64-unknown-linux-gnu`
* Linux (musl): `otaripper-x86_64-unknown-linux-musl`
* macOS (Intel): `otaripper-x86_64-apple-darwin`
* macOS (Apple Silicon): `otaripper-aarch64-apple-darwin`

## Arch Linux (AUR)

otaripper is available on the AUR:

```bash
paru -S otaripper        # build from source (recommended)
paru -S otaripper-bin    # prebuilt glibc binary

yay -S otaripper
yay -S otaripper-bin
```

If `otaripper-bin` fails to run due to libc/runtime issues, use the
fully static musl build from GitHub Releases:

[https://github.com/syedinsaf/otaripper/releases](https://github.com/syedinsaf/otaripper/releases)

---

## Basic Usage

Extract everything:

```bash
otaripper ota.zip
````

List partitions:

```bash
otaripper -l ota.zip
```

Extract selected partitions:

```bash
otaripper ota.zip -p boot,vendor_boot,init_boot
```

Print hashes:

```bash
otaripper ota.zip --print-hash
```

Strict verification:

```bash
otaripper ota.zip --strict
```

Disable automatic folder opening:

```bash
otaripper ota.zip -n
```

---

## Cleanup

Remove previously extracted folders:

```bash
otaripper clean
```

Clean a specific directory:

```bash
otaripper clean -o /path/to/output
```

The cleanup command only removes directories matching `extracted_*`
and refuses to operate on filesystem roots for safety.

---

## Command Options

| Option             | Description                         |
| ------------------ | ----------------------------------- |
| `-l, --list`       | List partitions only                |
| `-p, --partitions` | Extract specific partitions         |
| `-o, --output-dir` | Custom output directory             |
| `--strict`         | Enforce manifest hashes             |
| `--no-verify`      | Disable verification (unsafe)       |
| `--print-hash`     | Print SHA-256 hashes                |
| `--sanity`         | Detect obviously invalid output     |
| `--stats`          | Show performance statistics         |
| `-t, --threads`    | Thread control (1–256, 0 = auto)    |
| `-n, --no-open`    | Disable folder auto-open            |
| `clean`            | Remove `extracted_*` folders safely |

---

## Building from Source

### Requirements

* **Rust 1.93.0 or newer** (MSRV)
* Git
* C compiler (gcc / clang / MSVC) - required by some native dependencies

### Build

```bash
git clone https://github.com/syedinsaf/otaripper.git
cd otaripper
cargo build --release
```

Binary output:

* Linux/macOS: `target/release/otaripper`
* Windows: `target/release/otaripper.exe`

---


## Native Optimized Build (Advanced)

otaripper can be built locally with **CPU-specific optimizations** for maximum performance.
This enables all instruction sets supported by your CPU (AVX2 / AVX-512 / ARMv8, etc.).

⚠️ **Important:**
Binaries built this way are **NOT portable** and **must NOT be redistributed**.

---

### Linux / macOS (build.sh)

A helper script is provided to:
- download the source
- optionally install Rust (with confirmation)
- build a **CPU-native release binary**
- clean up all intermediate files

#### Requirements
- `curl`
- `unzip`
- A C toolchain (gcc / clang)
- Rust (installed automatically if missing)

#### Usage

```bash
chmod +x build.sh
./build.sh
````

Output binary:
After running `build.sh`, a new folder named `otaripper-native`
will be created **in the same directory where `build.sh` is located**.


```text
~/otaripper-native/otaripper
```

---

### Windows (PowerShell – MSVC)

On Windows, a native PowerShell script is provided.
It uses the official **Windows rustup installer** and defaults to the **MSVC toolchain**.

#### Requirements

* Windows 10 / 11
* PowerShell 5.1 or newer
* Visual Studio Build Tools (prompted automatically if missing)

#### Usage

Before running the script, allow execution **for the current session only**:

```powershell
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
```

Then run:

```powershell
.\build.ps1
```

Output binary:
After running `build.ps1`, a new folder named `otaripper-native`
will be created **in the same directory where `build.ps1` is located**.

```text
otaripper-native\otaripper.exe
```

---

### Notes

* Native builds use `-C target-cpu=native`
* Performance may be significantly higher than portable binaries
* These builds are intended for **local use only**
* GitHub Releases remain the recommended option for most users

---

## Contributing

Testing, bug reports, and performance feedback are welcome.

Please include:

* OS, CPU, RAM
* otaripper version
* OTA size and format
* logs or error messages if available

Pull requests should:

1. Build cleanly
2. Preserve safety guarantees
3. Avoid introducing undefined behavior
4. Keep performance regressions justified

---

## Acknowledgments

otaripper benefits greatly from real-world testing and feedback.

Special thanks to **Jean Rivera** for extensive validation, edge-case testing,
and correctness feedback.

Thanks also to:

* Android Open Source Project documentation
* Rust ecosystem maintainers
* Users who reported bugs and performance issues

---

## Show Your Support

If otaripper helped you:

* **Star the repository** — https://github.com/syedinsaf/otaripper
* **Report issues** — https://github.com/syedinsaf/otaripper/issues
* **Submit pull requests** — https://github.com/syedinsaf/otaripper/pulls
* **Share with others** — https://github.com/syedinsaf/otaripper

---

## License

otaripper is licensed under the **Apache License 2.0**.
See [LICENSE](LICENSE) for details.

---

## Disclaimer

Use at your own risk.

* Always verify extracted images before flashing
* Keep backups whenever possible
* Understand your device and bootloader requirements

The author and contributors are not responsible for data loss,
bricked devices, or damage resulting from misuse.

---
