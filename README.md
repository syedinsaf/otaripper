<!-- markdownlint-configure-file {
  "MD033": false,
  "MD041": false
} -->

<div align="center">

# otaripper

**Fast, safe, and reliable Android OTA partition extractor** <br />
*Extract partitions from Android OTA files with enterprise-grade verification and performance*

[![GitHub release](https://img.shields.io/github/v/release/syedinsaf/otaripper?style=for-the-badge)](https://github.com/syedinsaf/otaripper/releases)
[![Downloads](https://img.shields.io/github/downloads/syedinsaf/otaripper/total?style=for-the-badge)](https://github.com/syedinsaf/otaripper/releases)
[![License](https://img.shields.io/github/license/syedinsaf/otaripper?style=for-the-badge)](LICENSE)

[Download](https://github.com/syedinsaf/otaripper/releases) • [Usage](#quick-start) • [Build](#building-from-source) • [Advanced Usage](#Advanced-Features) 

</div>

---

## Why otaripper?

otaripper stands out with comprehensive safety features and performance optimizations that other tools lack. Unlike alternatives, it verifies both input files AND extracted outputs, preventing corrupted partitions that could brick your device.

---

## Technical Overview

otaripper is a systems-level utility designed to saturate hardware throughput while maintaining absolute data integrity. Unlike standard extraction scripts, it leverages modern CPU instruction sets and rigorous memory-safety validation to ensure that extracted partitions are bit-perfect.

## Core Pillars

* **SIMD-Accelerated Core** Hand-tuned intrinsics for `AVX-512`, `AVX2`, and `SSE2` ensure data movement at the physical limits of the hardware.

* **Cryptographic Rigor** Implementation of double-ended verification—validating both the source payload hashes and the final written output against manifest `SHA-256` signatures.

* **Intelligent I/O Architecture** Utilizes Memory-Mapped I/O (`mmap`) and `MiMalloc` to eliminate unnecessary heap allocations and minimize kernel context switching.

* **Forensic Inspection** A sophisticated telemetry mode identifies partition structures (`Full` vs. `Incremental`) to prevent the generation of invalid images from patch files.

---

### Feature Comparison

|                                    | [syedinsaf/otaripper]        | [ssut/payload-dumper-go]  | [vm03/payload_dumper] |
| ---------------------------------- | --------------------- | ----------------- | -------------- |
| Input file verification            | ✅                     | ✅                 | ❌              |
| **Output file verification**       | ✅          | ❌                 | ❌              |
| SIMD-optimized operations          | ✅           | ❌                 | ❌              |
| Automatic error cleanup            | ✅           | ❌                 | ❌              |
| Performance statistics             | ✅           | ❌                 | ❌              |
| Extract selective partitions       | ✅                     | ✅                 | ✅              |
| Parallelized extraction            | ✅                     | ✅                 | ❌              |
| Direct .zip file support           | ✅                     | ✅                 | ❌              |
| Graceful Ctrl+C handling           | ✅           | ❌                 | ❌              |
| Real-time progress indicators      | ✅                     | ✅                 | ❌              |
| Incremental OTA support            | ❌          | ❌                 | ✅      |

> **Safety First**: otaripper is the only tool that verifies extracted partition integrity, preventing potentially dangerous corrupted files from reaching your device.

---

## Quick Start

### Basic Usage

```bash
# Drag & drop support - just run with your OTA file! or you run the below commands in CMD/Terminal
otaripper ota.zip                    # Windows
./otaripper ota.zip                  # Linux/macOS

# List partitions without extracting
otaripper -l ota.zip

# Extract specific partitions only
otaripper ota.zip --partitions boot,init_boot

# Specify OTA file with -p and output directory with -o
otaripper -p path/to/ota.zip -o path/to/output_dir
```

### Common Workflows

<details>
<summary><strong>Custom Recovery Development</strong></summary>

```bash
# Extract boot partitions for recovery development
./otaripper ota.zip --partitions boot,recovery,vendor_boot --print-hash
```
</details>

<details>
<summary><strong>ROM Development</strong></summary>

```bash
# Extract system partitions with verification
./otaripper ota.zip --partitions system,system_ext,product,vendor --strict --stats
```
</details>

<details>
<summary><strong>Quick Boot Image Extraction</strong></summary>

```bash
# Fast boot extraction 
./otaripper ota.zip --partitions boot
```
</details>

<details>
<summary><strong>Forensic Analysis</strong></summary>

```bash
# Maximum safety with all verification enabled
./otaripper ota.zip --strict --print-hash --plausibility-checks --stats
```
</details>

---

## Advanced Features

### Safety & Integrity

<table>
<tr>
<td><strong>--strict</strong></td>
<td>
<ul>
<li>Enforces cryptographic verification for ALL operations</li>
<li>Fails immediately if any hash is missing from manifest</li>
<li>Cannot be combined with <code>--no-verify</code></li>
<li><strong>Stops extraction and cleans up on any failure</strong></li>
</ul>
</td>
</tr>
<tr>
<td><strong>--print-hash</strong></td>
<td>
<ul>
<li>Displays SHA-256 hash for each extracted partition</li>
<li>Format: <code>boot: sha256=a1b2c3d4...</code></li>
<li>Perfect for verification and record-keeping</li>
<li>Reuses computed hashes when possible (no extra passes)</li>
</ul>
</td>
</tr>
<tr>
<td><strong>--plausibility-checks</strong></td>
<td>
<ul>
<li>Detects obviously corrupted output (e.g., all-zero images)</li>
<li>Lightweight checks with near-zero overhead</li>
<li>Catches issues even when manifest hashes are missing</li>
<li>Prevents flashing invalid partition images</li>
</ul>
</td>
</tr>
<tr>
<td><strong>--no-verify</strong></td>
<td>
<ul>
<li><strong>Use with extreme caution!</strong></li>
<li>Skips all hash verification (faster but dangerous)</li>
<li>Only recommended for trusted sources or debugging</li>
<li>Cannot be used with <code>--strict</code></li>
</ul>
</td>
</tr>
</table>

### Performance & Monitoring

<table>
<tr>
<td><strong>--stats</strong></td>
<td>Shows detailed performance metrics after extraction</td>
</tr>
<tr>
<td><strong>--threads N</strong></td>
<td>Control worker threads (1-256, or 0 for auto-detect)</td>
</tr>
<tr>
<td><strong>--output-dir PATH</strong></td>
<td>Custom output location (creates timestamped subdirectory)</td>
</tr>
<tr>
<td><strong>--no-open-folder</strong></td>
<td>Don't auto-open extracted folder when complete</td>
</tr>
</table>

### User Experience

- **Drag & Drop**: Simply drag OTA files onto the executable
- **Safe Interruption**: Ctrl+C gracefully stops and cleans up partial files  
- **Real-time Progress**: Live progress bars for all partitions
- **Auto-open**: Automatically opens extraction folder when complete
- **Smart Cleanup**: Removes partial files on any error
- **Helpful Errors**: Clear messages with actionable solutions

---

## Performance Optimizations

### SIMD Acceleration
**Automatic CPU optimization** - otaripper detects and uses the best available instruction set:

| CPU Feature | Performance Gain | Supported CPUs |
|-------------|------------------|----------------|
| **AVX-512** | Up to 8x faster | Intel Skylake-X+, AMD Zen 4+ |
| **AVX2** | Up to 4x faster | Intel Haswell+, AMD Excavator+ |
| **SSE2** | Up to 2x faster | Intel Pentium 4+, AMD Athlon 64+ |
| **Scalar** | Baseline | All other CPUs |

<details>
<summary><strong>Debug CPU detection</strong></summary>

```bash
# See what CPU features are detected
OTARIPPER_DEBUG_CPU=1 ./otaripper ota.zip
```
</details>

### Memory Optimizations
- **Memory-mapped I/O**: Efficient file access without loading entire files
- **In-memory ZIP processing**: No temporary files for ZIP extraction
- **Extent coalescing**: Reduces memory copy operations by 60-80%
- **Cache-friendly chunking**: 64KB optimal chunk size for better performance

### Smart Hashing
**Inline hashing** eliminates redundant passes for large partitions (>256 MiB):

```bash
# Environment controls for advanced users
export OTARIPPER_INLINE=auto    # Use size heuristic (default: off)
export OTARIPPER_INLINE=on      # Force inline hashing
export OTARIPPER_INLINE=off     # Force post-pass hashing
```

---

## Output Examples

### Successful Extraction
```
Processing 12 partitions using 16 threads...

           boot [████████████████████] 100%
    vendor_boot [████████████████████] 100%
         system [████████████████████] 100%
      system_ext [████████████████████] 100%

Extraction completed successfully!
Output directory: /home/user/extracted_20241225_143022
Total extracted size: 8.2 GB
Tool Source: https://github.com/syedinsaf/otaripper
```

### With Hash Verification
```bash
./otaripper ota.zip --print-hash
```
```
Partition hashes (SHA-256):
boot: sha256=a1b2c3d4e5f67890abcdef1234567890abcdef1234567890abcdef1234567890
vendor_boot: sha256=b2c3d4e5f67890a1bcdef1234567890abcdef1234567890abcdef1234567890a
system: sha256=c3d4e5f67890a1b2cdef1234567890abcdef1234567890abcdef1234567890ab
```

### Performance Statistics
```bash
./otaripper ota.zip --stats
```
```
Extraction statistics:
  - boot: 64.0 MB in 45 ms (1.42 GB/s)
  - vendor_boot: 128.0 MB in 67 ms (1.91 GB/s)
  - system: 2.1 GB in 1205 ms (1.74 GB/s)
  Total: 2.29 GB in 1317 ms (1.74 GB/s)
```

---

## Building from Source

### Prerequisites
- **Rust 1.82.0+** with Cargo
- **Git** for cloning

### Build Commands

```bash
# Clone and build (optimized release)
git clone https://github.com/syedinsaf/otaripper.git
cd otaripper
cargo build --release

# Platform-specific optimizations
cargo build --release --features asm-sha2  # Linux/macOS only
```

### Build Features

| Feature | Description | Compatibility |
|---------|-------------|---------------|
| `asm-sha2` | Assembly-optimized SHA-256 hashing | ✅ Linux/macOS<br>❌ Windows MSVC/GNU |
| `default` | Standard optimizations | ✅ All platforms |

**Output Location:**
- Linux/macOS: `target/release/otaripper`  
- Windows: `target/release/otaripper.exe`

---

## Advanced Usage

### Power User Examples

<details>
<summary><strong>Maximum Safety Extraction</strong></summary>

```bash
# Enterprise-grade extraction with all safety features
./otaripper ota.zip \
  --strict \
  --print-hash \
  --plausibility-checks \
  --stats \
  --output-dir /secure/location
```
</details>

<details>
<summary><strong>Performance-Optimized Extraction</strong></summary>

```bash
# Maximum speed extraction (trusted source only)
./otaripper ota.zip \
  --no-verify \
  --no-open-folder \
```
</details>

<details>
<summary><strong>Development & Debugging</strong></summary>

```bash
# Detailed analysis with debug info
OTARIPPER_DEBUG_CPU=1 OTARIPPER_INLINE=auto ./otaripper ota.zip \
  --stats \
  --print-hash \
```
</details>

### Environment Variables

| Variable | Values | Description |
|----------|--------|-------------|
| `OTARIPPER_DEBUG_CPU` | `1` | Show CPU feature detection |
| `OTARIPPER_INLINE` | `on\|off\|auto` | Control inline hashing strategy |

---

## FAQ

<details>
<summary><strong>Q: Is it safe to interrupt otaripper with Ctrl+C?</strong></summary>

**A:** Yes! otaripper handles interruption gracefully:
- Stops all worker threads safely
- Removes any partially extracted files
- Cleans up temporary directories if created
- Shows clear cleanup status messages
</details>

<details>
<summary><strong>Q: Why does otaripper create timestamped folders?</strong></summary>

**A:** Timestamped folders prevent accidental overwrites and help track multiple extractions. Each run gets a unique folder like `extracted_20241225_143022`.
</details>

<details>
<summary><strong>Q: What's the difference between --strict and normal mode?</strong></summary>

**A:** 
- **Normal mode**: Verifies hashes when available, continues if missing
- **--strict mode**: REQUIRES all hashes to be present, fails immediately if any are missing
- Use `--strict` for maximum safety, normal mode for compatibility
</details>

<details>
<summary><strong>Q: How much faster is otaripper compared to other tools?</strong></summary>

**A:** Performance varies by system, but typical improvements:
- **2-4x faster** than payload_dumper (Python(C really)based)
- **20-50% faster** than payload-dumper-go (again C)
- **Up to 8x faster** on AVX-512 capable CPUs for large operations
</details>

## License

This project is licensed under the **MIT License** - see the [LICENSE](LICENSE) file for details.

---

## Acknowledgments

- **Android Open Source Project** for OTA format specifications
- **Rust Community** for excellent crates and tooling
- **Contributors** who help make otaripper better


## Special Thanks

Heartfelt thanks to [Jean Rivera](https://github.com/jeanrivera) for thorough testing and feedback during development. Your help made this project better!

[Star this repo](https://github.com/syedinsaf/otaripper/stargazers) •  [Report Issues](https://github.com/syedinsaf/otaripper/issues)  •  [Submit Pull Requests](https://github.com/syedinsaf/otaripper/pulls)

</div>


**Made with love by [Syed Insaf](https://github.com/syedinsaf)**


<div>

## ⚠️ Disclaimer


**Important:**  Use this tool at your own risk. The author **assumes no responsibility** for any damages, data loss, or other issues caused directly or indirectly by the use of this software. Please ensure you understand the risks and take appropriate precautions (such as backing up your data) before using.

By using this tool, you agree that you will not hold the author or maintainers liable for any damages or losses.

</div>


[syedinsaf/otaripper]: https://github.com/syedinsaf/otaripper
[ssut/payload-dumper-go]: https://github.com/ssut/payload-dumper-go
[vm03/payload_dumper]: https://github.com/vm03/payload_dumper
