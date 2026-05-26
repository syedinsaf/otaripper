# Technical Documentation

This document provides detailed technical information about **otaripper’s** architecture, design decisions, and implementation details.

> **v3.2.1 Note:**
> This release introduces Parallel Chunked HTTP Streaming, massive network resilience hardening, smart firmware metadata extraction, dynamic folder naming, simplified terminal log outputs, and GUI terminal silencing.

---

## Table of Contents

* [Architecture Overview](#architecture-overview)
* [Memory Management](#memory-management)
* [SIMD Optimization](#simd-optimization)
* [Remote Extraction Architecture](#remote-extraction-architecture)
* [Verification Pipeline](#verification-pipeline)
* [Qualcomm Bootloader ARB Analysis](#qualcomm-bootloader-arb-analysis)
* [Parallel Extraction](#parallel-extraction)
* [Reliability and Failure Handling](#reliability-and-failure-handling)
* [Performance Architecture](#performance-architecture)
* [Advanced Configuration](#advanced-configuration)
* [Design Decisions](#design-decisions)
* [Release Infrastructure](#release-infrastructure)
* [Future Optimizations](#future-optimizations)
* [Troubleshooting](#troubleshooting)
* [References](#references)

---

## Architecture Overview

otaripper is structured around three core principles, refined in v2.1 to favor **predictable performance** over generalized abstractions:

1. **Safety First**
   Memory safety enforced by Rust’s type system, strict validation, and explicit lifetime control.

2. **Zero-Copy I/O**
   Memory-mapped file operations to minimize data movement and kernel/user transitions.

3. **Contention-Free Concurrency**
   Parallel extraction with workers operating exclusively on disjoint memory regions.

### Key Components

* **Payload Parser** — Parses Android OTA manifests and payload structures
* **Memory Mapper** — Manages memory-mapped I/O for payloads and output partitions
* **Worker Pool** — Executes extraction operations in parallel
* **Verification Engine** — SHA-256 validation and sanity checking
* **Progress Monitor** — Lock-free progress tracking with minimal redraw overhead

### Code Structure (Modular Refactor)

To improve maintainability and performance isolation, the underlying operation code is cleanly decoupled:
* `src/cmd/mod.rs` — CLI argument parsing, subcommands, and high-level orchestration.
* `src/cmd/extractor.rs` — Core extraction logic, mmap handling, concurrent worker pool coordination, and zero-copy data routing.
* `src/cmd/simd.rs` — Platform-specific SIMD execution paths, CPU detection, and block-optimized copy routines.

---

## Memory Management

### Zero-Copy Zip Memory Mapping (v2.3)

Prior to v2.3, extracting partitions from a `.zip` file required unpacking the `payload.bin` to a massive temporary file first. 

otaripper now inspects the zip file metadata. If the payload uses the `STORED` compression method (which almost all Android OTA zips do), it safely bypasses the temporary file entirely. Instead, it memory-maps the *entire* `.zip` file directly from the disk and provides the internal parser with a sliced view starting at the precise byte offset of `payload.bin`.

This optimization yields a 2x reduction in overall SSD write cycles per extraction and eliminates startup delays.

### File Memory Mapping

otaripper avoids traditional buffered I/O in favor of memory mapping:

```
Traditional:
  File → read() → Kernel Buffer → User Buffer → Copy → Process

otaripper:
  File → mmap() → Direct Process Access
```

**Benefits**

* Zero-copy reads
* OS-managed page cache
* Lower memory pressure
* Safe concurrent reads via read-only mappings

**Implementation Details**

* Input payload: read-only `mmap`
* Output partitions: write-only `mmap` with pre-allocation
* Strict extent validation before any write occurs
* Page-aligned access patterns (typically 4 KB)

---

### Memory Layout

```
┌─────────────────────────────────────┐
│ Input: payload.bin (read-only mmap) │
│ - Shared across all workers         │
│ - OS-managed paging                │
└─────────────────────────────────────┘
                ↓
┌─────────────────────────────────────┐
│ Worker Threads (N parallel)         │
│ - Disjoint extents only             │
│ - No locks in hot path              │
│ - Thread-local decompression        │
│ - 1 MiB thread-local buffer pool    │
└─────────────────────────────────────┘
                ↓
┌─────────────────────────────────────┐
│ Output: partition.img (write mmap)  │
│ - Pre-sized to final length         │
│ - No overlapping writes             │
└─────────────────────────────────────┘
```

---

### Fast-Path Write Specialization / Zero-Copy Decompression

otaripper implements a strict fast path for the most common case:
**operations that target exactly one contiguous destination extent**.

When reading or decompressing (e.g., bzip2, xz) into a single extent, the data is pushed **directly into the memory-mapped file**, skipping intermediary buffering altogether.

This single-extent zero-copy path eliminates:
* Redundant buffering and copying round-trips
* Iterator overhead and per-extent bounds checks

**Data Integrity Verification**: Because the zero-copy fast path streams straight to the memory map, otaripper intentionally forces the decompressor to hit EOF. This guarantees trailing CRC/checksum logic in the underlying compression stream is evaluated and correctly bubbles up any underlying I/O corruption errors.

---

### Thread-Local Buffer Pooling

For multi-extent writes where zero-copy streaming isn't possible, memory allocation overhead is minimized via a thread-local buffer pool (`COPY_BUFFER`). Rayon workers share 1 MiB buffers which amortize allocation costs across iterative decompression tasks and provide a sufficiently large chunk size to safely trigger SIMD non-temporal streaming writes on the output.

---

## SIMD Optimization

### Automatic CPU Detection

CPU capabilities are detected once at startup and cached globally:

```
Priority Order:
  AVX-512 (512-bit)
  AVX2    (256-bit)
  SSE2    (128-bit)
  Scalar  (fallback)
```

Detection uses `is_x86_feature_detected!` and is fully runtime-safe.

---

### SIMD Applications

**1. Memory Copying**

* SIMD-accelerated block copying
* Streaming non-temporal stores for large write-once buffers
* Chunked writes to avoid long pipeline stalls
* 16–64 bytes per instruction depending on SIMD width

**2. All-Zero Detection**

* SIMD-accelerated sanity checks
* Near-zero overhead
* Detects obviously invalid images (e.g. all-zero partitions)

**3. Hashing**

SHA-256 uses a constant-time, standards-compliant implementation.
Hashing is not SIMD-parallelized; in practice, I/O and decompression dominate runtime.

---

### Cache-Aware Write Thresholds (v2.1)

otaripper uses a **1 MiB threshold** to decide when to use streaming (non-temporal)
SIMD stores instead of normal cached writes.

This value is chosen because:

* It exceeds typical L2 cache sizes
* It amortizes SIMD setup and fencing costs
* Writes of this size are almost always write-once
* Avoids evicting hot metadata and worker state from cache

For smaller writes, cached stores are faster due to lower latency.

This heuristic is intentionally conservative and tuned for real OTA workloads.

---

### Debug CPU Detection

```bash
OTARIPPER_DEBUG_CPU=1 ./otaripper ota.zip
```

Outputs detected SIMD capabilities and the selected execution path.

---

## Remote Extraction Architecture

otaripper's remote HTTP extraction engine (`--remote`) is highly tuned to bypass OS-specific networking quirks while maintaining absolute reliability across varied native environments.

### Parallel Chunked Streaming (v3.1)

To saturate high-bandwidth connections, `otaripper` no longer fetches remote partitions as a single linear stream. Instead, it utilizes **Parallel Chunked Requests**:
* Large partitions are decomposed into **8 MiB chunks**.
* These chunks are distributed across the Rayon worker pool.
* Each worker issues independent `Range` requests, allowing the engine to overcome per-connection bandwidth throttling often implemented by CDNs.

### Network Resilience and Offline Recovery

Remote extraction is inherently volatile. `otaripper` implements a robust recovery loop:
* **Exponential Backoff**: Failed requests are retried indefinitely with a backoff starting at 500ms and capping at 3s.
* **Stateful Resumption**: The engine tracks exactly which bytes were successfully read and validated, resuming precisely at the point of failure.
* **Dynamic Network Monitoring**: A global bandwidth tracker (`NETWORK_BYTES_READ`) provides real-time throughput data to a dedicated UI thread, which displays a separate "Network" progress bar.
* **429 Rate Limit Handling**: If a server issues a "Too Many Requests" response, `otaripper` automatically pauses, marks the thread as offline, and waits for a cooldown period before resuming.

### Dynamic DNS Resolution

1. **Linux / Windows (`hickory-dns`)**
   On statically-linked Linux (`musl`) and Windows, otaripper uses the pure-Rust `hickory-dns` resolver. This explicitly bypasses a known bug in `musl libc` where cold HTTP connections were unexpectedly dropped due to strict native timeout handling.
2. **Android CLI (Native Resolver Fallback)**
   On Android, the pure-Rust resolver is disabled via a dynamic `/system/build.prop` existence check. This forces otaripper to use Android's native OS resolver. This fallback is critical for preventing crashes on custom ROMs with broken or missing `/etc/resolv.conf` symlinks.

### HTTP Stack Profiling

otaripper actively pins the `reqwest` HTTP engine below v0.13 to avert a breaking transition to Java-based JNI platform verifiers. This guarantees otaripper remains a completely pure native binary, capable of running inside an Android terminal emulator (Termux, adb shell) without triggering Java runtime panics.

---

## Verification Pipeline

otaripper implements a three-layer verification system.

### Layer 1: Input Validation (Always Enabled)

* Protobuf structure validation
* Manifest consistency checks
* Extent boundary verification
* Block-size sanity checks

Purpose: reject malformed or corrupted inputs before extraction begins.

---

### Layer 2: Operation Verification (Default)

* Data hash verification (if present)
* Decompression integrity
* Safe write enforcement

Disabled only with `--no-verify`.

---

### Layer 3: Output Verification (Default)

* Final SHA-256 verification
* Optional sanity checks (`--sanity`)
* Strict enforcement with `--strict`

---

### Verification Modes

| Mode          | Input | Ops | Output      | Use Case        |
| ------------- | ----- | --- | ----------- | --------------- |
| Default       | ✅     | ✅   | ✅           | Normal use      |
| `--strict`    | ✅     | ✅   | enforced    | Maximum safety  |
| `--no-verify` | ✅     | ❌   | ❌           | Trusted sources |
| `--sanity`    | ✅     | ✅   | +zero-check | Analysis        |
---

## Qualcomm Bootloader ARB Analysis

The `arb` (or `arbscan`) subcommand performs low-level ELF structural analysis and Secure Boot integrity checks on Qualcomm Extensible Bootloader configurations (`xbl_config.img`). 

### 1. Zero-Download Payload Extraction
When given a full local or remote OTA ZIP package, `otaripper` does not unpack the entire payload. Instead, it coordinates with the `Extractor` to stream only the exact block ranges of the tiny `xbl_config` partition from the remote server, performing the analysis within milliseconds.

### 2. ELF Header & Program Header Table Parsing
* **Magic Validation**: The engine inspects the ELF identification bytes (`\x7fELF`) to verify a valid little-endian ELF64 layout (`ELFCLASS64`, `ELFDATA2LSB`).
* **Program Header Navigation**: Extracts the program header table offset (`e_phoff`) and entries count (`e_phnum`).
* **Segment Filtering**: Iterates through program segments under **20 MB** (`MAX_SEGMENT_SIZE`) to filter candidates for the Secure Boot `HASH` metadata partition, enforcing that the target segment is non-executable (`(p_flags & 0x1) == 0`).

### 3. Multi-Digest Secure Boot Verification (SHA-256 / SHA-384)
To parse the Secure Boot hash table header safely, the engine scans the program segment for the structured Qualcomm `HASH` header signature block:
* **The Constraint**: The hash table size must be fully aligned with the cryptographic digest width used by the bootloader.
* **Modern SoC Adaptability**: Older Snapdragon chips strictly used **SHA-256** (32-byte digests), meaning `hash_tbl_sz` was always a multiple of 32 (`(hash_tbl_sz & 0x1F) == 0`). Modern Qualcomm SoCs (such as Snapdragon 8 Gen 1, Gen 2, and Gen 3) utilize **SHA-384** (48-byte digests) for Secure Boot.
* **Universal 16-Byte Mask**: To support both legacy and modern SoC architectures seamlessly without code duplication or extra libraries, the validation routine evaluates the size against a **16-byte alignment mask**:
  $$\text{hash\_tbl\_sz} \pmod{16} \equiv 0 \implies (\text{hash\_tbl\_sz} \ \& \ \text{0xF}) == 0$$
  This allows both SHA-256 (32) and SHA-384 (48) tables to pass validation, and future-proofs the CLI for potential SHA-512 (64) bootloaders.

### 4. OEM Metadata Retrieval
Once the `HASH` header is located, the parser computes the offset for the OEM metadata block:
$$\text{Offset}_{\text{OEM}} = \text{Offset}_{\text{HASH}} + 36 + \text{common\_sz} + \text{qti\_sz}$$
It reads the little-endian fields to retrieve the target:
* `Major Version`
* `Minor Version`
* `ARB Index (Anti-Rollback Level)`

### 5. Smart Auto-Detection & Sanitized Export
When writing JSON outputs, the engine pre-processes inputs to ensure strict filesystem safety:
* **Firmware Auto-Detection**: Automatically parses `META-INF/com/android/metadata` to extract the OS version, skipping manual prompts and writing directly to `CPH2573_15.0.0.860(EX01)_ARB(N).json`.
* **CDN URL Expiry Cleansing**: Strips protocol schemes and complex query string parameters (which contain illegal Windows filesystem characters like `?`, `&`, or `=`) before generating the output filename.
* **User Input Sanitization**: Pre-cleanses spaces, slashes, and backslashes (e.g. `16.0.7.500\`) to eliminate duplicate underscores in the generated filename.
* **ARB Level Suffixing**: Automatically appends the actual Anti-Rollback index to the final filename (as `_ARB(N).json`), making the level instantly readable on disk.

---

## Parallel Extraction

### Contention-Free Design

Workers operate on **disjoint memory regions** proven safe by upfront validation.

```
Main Thread:
  Parse → Validate → mmap → Spawn workers

Worker:
  Read → Decompress → Write → Progress update
```

### Why This Is Safe

* Non-overlapping extents validated before execution
* Read-only payload mapping
* Write-only output mapping
* Scoped threads prevent lifetime violations

Extraction is aborted *before any write occurs* if overlapping extents are detected.

---

### Thread Pool Configuration

* Auto-detected by default
* Manually configurable via `-t`
* Benefits taper beyond ~16 threads on most systems
* SSDs scale better than HDDs

---

## Reliability and Failure Handling

### Error Handling Philosophy

otaripper follows **fail-fast, clean-up always** semantics.

### Transactional Extraction Semantics

* On failure or interruption:

  * All created partition files are deleted
  * Output directory is removed if created by otaripper
* On success:

  * All outputs remain intact

No partial or ambiguous state is ever left behind.

---

## Performance Architecture

### Common Bottlenecks

1. Storage I/O (most common)
2. Decompression (bzip2/xz)
3. CPU (least common)

### Optimization Strategies

1. Memory-mapped I/O (Files and Zip Offsets)
2. Contention-free parallelism
3. SIMD acceleration
4. Extent coalescing
5. **Hot-path specialization (v2.1)**

---

### Built-in Statistics (`--stats`)

Reports per-partition and total throughput to identify bottlenecks.

---

## Advanced Configuration

### Fastboot Firmware Flasher Integration (--fff)

When the `--fff` command-line flag is active, `otaripper` structures the output partition files for compatibility with Fastboot Firmware Flasher scripts.

#### Standard Extraction Mode
Instead of extracting all `.img` files directly to the root of the output directory, it:
1. Dynamically parses the target device version (e.g. `CPH2573_15.0.0.860(EX01)`) from the firmware metadata (falling back to `"firmware"` if unavailable).
2. Resolves each partition to its respective FFF subdirectory:
   - `BOOTLOADER`: `boot`, `init_boot`, `recovery`, `vbmeta`, `vbmeta_system`, `vbmeta_vendor`, `vendor_boot`
   - `CRITICAL`: `abl`, `aop`, `aop_config`, `bluetooth`, `cpucp`, `cpucp_dtb`, `devcfg`, `dsp`, `dtbo`, `engineering_cdt`, `featenabler`, `hyp`, `imagefv`, `keymaster`, `oplus_sec`, `oplusstanvbk`, `qupfw`, `shrm`, `splash`, `tz`, `uefi`, `uefisecapp`, `xbl`, `xbl_config`, `xbl_ramdump`
   - `MODEM`: `modem`
   - `SYSTEM`: `my_bigball`, `my_carrier`, `my_engineering`, `my_heytap`, `my_manifest`, `my_product`, `my_region`, `my_stock`, `odm`, `product`, `system`, `system_dlkm`, `system_ext`, `vendor`, `vendor_dlkm`
   - `EXTRA`: Default fallback for any other partitions
3. Creates the nested directory `<output_dir>/<device_version>/<subfolder>` on-demand, preventing empty directories if certain partitions are not being extracted.

#### Directory Reorganization Mode
If the positional argument is a directory instead of a payload file/URL, and `--fff` is specified, `otaripper`:
1. Parses the device name from the input directory name (by removing any `extracted_` prefix and `_YYYY-MM-DD_HH-MM-SS` timestamp suffix).
2. Creates a sibling directory next to the original folder, named after the device version (e.g. `CPH2573_15.0.0.860(EX01)`).
3. Groups and moves all `.img` files located at the root of the original directory into their respective category folders (`BOOTLOADER`, `CRITICAL`, etc.) inside the new sibling directory.
4. Prompts the user before deleting the original folder once the relocation is complete.

### Environment Variables

* `OTARIPPER_DEBUG_CPU` — show SIMD selection

### Build-Time Optimizations

```toml
[profile.release]
lto = "fat"
codegen-units = 1
```

Optional `target-cpu=native` for local builds.

---

## Design Decisions

### Why mmap?

* Zero-copy
* OS-managed caching
* Simplified correctness model

### Why Contention-Free Writes?

* No locks in hot path
* Predictable performance
* Strong correctness guarantees

### Why Rust?

* Memory safety without GC
* Zero-cost abstractions
* Strong tooling for systems work

---

## Release Infrastructure

otaripper’s GitHub Actions CI pipeline implements an industry "Gold Standard" verification loop to ensure absolute supply-chain integrity:

### Two-Layer Checksum Architecture
1. **The External Archive Hash (`checksums.txt`)**
   The pipeline compiles a single master `checksums.txt` file containing the hashes of all `.tar.gz` and `.zip` artifacts. This acts as the canonical source of truth for automated package managers (like Winget and AUR) to verify the download before proceeding with a build recipe.
2. **The Internal Binary Hash (`otaripper.sha256`)**
   A separate `.sha256` file containing the hashes of the raw executables is placed *inside* the release archive. This allows a user to verify the integrity of the binaries locally after extraction, without causing cross-platform filename hash collisions (`otaripper` vs `otaripper.exe`) in the master checksum file.

### Lite Builds
The CI automatically compiles `otaripper-lite` binaries (`--no-default-features`) alongside standard builds. These strip away the `reqwest` networking stack, offering a minimal, zero-dependency alternative for users who only extract local files.

---

## Future Optimizations

* Incremental OTA support
* Optional GUI frontend for visualization and inspection

---

## Troubleshooting

### Slow Extraction

Likely causes:

* HDD I/O
* Heavy compression
* Excessive thread count

### Hash Failures

Likely causes:

* Corrupted OTA
* Disk issues
* Rare hardware faults

---

## References

* Android OTA Format — [https://source.android.com/devices/tech/ota](https://source.android.com/devices/tech/ota)
* `mmap(2)` — [https://man7.org/linux/man-pages/man2/mmap.2.html](https://man7.org/linux/man-pages/man2/mmap.2.html)
* Intel Intrinsics Guide — [https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html](https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html)
* The Rust Book — [https://doc.rust-lang.org/book/](https://doc.rust-lang.org/book/)

---

For user-facing documentation, see [README.md](README.md)

---
