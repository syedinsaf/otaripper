# Changelog

## **otaripper v2.2.2** (2026-05-05)

### Arbscan Integration

This release merges the standalone `arbscan` utility directly into the `otaripper` codebase as a native subcommand.

---

## New Features

* **`otaripper arbscan` subcommand**
  * Built-in support for analyzing OEM Anti-Rollback (ARB) metadata from Qualcomm bootloader images (e.g., `xbl_config.img`).
  * Extracts Major/Minor versions and ARB index automatically.
  * Optionally outputs JSON metadata for further automation.
  * Eliminates the need to maintain a separate binary for bootloader analysis.

---
## **otaripper v2.2.1** (2026-05-04)

### Modern Decompression & ARM64 Support

This release upgrades the decompression engine to support modern Android OTA payloads using the ARM64 BCJ filter, resolving decompression failures on newer devices like Xiaomi HyperOS.

---

## Decompression Engine

* **Upgraded to `liblzma v0.4.6`**
  * Replaced `xz2` with a maintained `liblzma` fork (XZ 5.8 backend).
  * Fixed decompression crashes when extracting modern OTAs.
  * Ensures full support for ARM64 BCJ filters across all platforms.
* **Musl Compatibility**
  * Maintained robust static linking for `musl` builds, ensuring highly portable Linux binaries.

---

## CI & Infrastructure

* **GitHub Actions Modernization**
  * Upgraded all CI workflows to latest versions (Version 5/6+).
  * Eliminated Node.js deprecation warnings by migrating to Node 24-powered actions.
  * Improved build runner reliability and verification speed.

---

## Credits

Special thanks to **ArKT-7** for the critical contribution of `liblzma` modernization and ARM64 payload support.

---

## **otaripper v2.2.0** (2026-05-04)

### Architectural & Performance Refactor

This major release re-architects the extraction engine for significantly improved scalability and performance on modern high-core-count systems.

---

## Engine Refactoring

* **Modular Architecture**
  * Decoupled monolithic logic into specialized `extractor` and `simd` modules.
  * Improved maintainability and performance isolation of platform-specific code.
* **Thread-Local Buffer Pooling**
  * Drastically reduced memory allocation overhead via thread-local buffer reuse in Rayon workers.
  * Enables purely alloc-free zero-copy decompression paths.
* **SIMD Optimization**
  * Optimized non-temporal cache-bypassing mechanisms for `AVX-512`, `AVX2`, and `SSE2`.
  * Improved runtime CPU feature detection.

## Build & Tooling

* **Native Build Scripts**
  * Added `build.sh` (Linux/macOS) and `build.ps1` (Windows) for local, CPU-optimized builds.
* **Protobuf Modernization**
  * Refactored protobuf module structure for better build-time stability.

---

## **otaripper v2.1.0** (2026-04-30)

### Performance & Architecture Upgrade

This release is a **deep performance, scalability, and correctness refinement** of v2.0.0.
No user workflows were broken, but large internal parts of the extraction engine were **re-architected for speed, cache efficiency, and parallel scalability**, especially on modern CPUs.

---

## Highlights

* Significantly faster extraction on large partitions
* Lower CPU cache pollution on write-once workloads
* Reduced synchronization overhead under heavy parallelism
* More predictable performance across CPUs (AVX-512 / AVX2 / SSE2 / scalar)
* New maintenance subcommand for cleanup

---

## New Features

* **`otaripper clean` subcommand**

  * Safely removes `extracted_*` directories
  * Optional target directory
  * Interactive confirmation
* Byte-accurate progress bars (tracks actual bytes written, not operations)
* Automatic detection of zero-heavy partitions for optimized handling

---

## Major Performance Improvements

### Extraction Fast Paths

* **Single contiguous extent fast path**

  * Bypasses generic extent writer when possible
  * Direct slice copy for small buffers
  * SIMD streaming copy for large buffers
* Eliminates unnecessary per-extent loops and bounds checks in hot paths

### SIMD & Memory Copy Engine

* Centralized SIMD dispatch via explicit `CpuSimd` selection
* Improved non-temporal (streaming) store usage for large write-once buffers
* Tuned thresholds and chunk sizes for modern CPUs
* Reduced runtime feature detection overhead
* Better cache behavior under multi-threaded extraction

### Threading & Scheduling

* New shared **WorkerContext** reduces `Arc` cloning and contention
* Smarter chunked scheduling for large operation sets
* Serial fast path for very small operation counts
* Improved load balancing across partitions

---

## Zero / Discard Optimization

* Detects partitions dominated by Zero / Discard operations
* Performs a single mmap fill instead of repeated zero writes
* Zero operations can become true no-ops when safe
* Large speedups on sparse vendor / product images

---

## Reliability & Safety Improvements

* Stronger block size validation (including power-of-two enforcement)
* Safer pointer handling with explicit `PartitionPtr` invariants
* Simplified and robust non-overlapping extent validation
* Corrected SIMD zero-detection logic on AVX-512 and AVX2
* More predictable cleanup behavior on failure or cancellation
* Clearer unsafe boundaries with documented guarantees

---

## UX / CLI Improvements

* Progress bars reflect real throughput (bytes, not op count)
* Cleaner error propagation on multi-threaded failures
* More consistent cancellation behavior under load
* Improved help text including cleanup workflow

---

## Build & Platform Improvements

* Improved sequential IO hints on Linux (`posix_fadvise`, `madvise`)
* Better mmap write behavior for large outputs
* Internal tuning for high-core-count systems
* Continued full support for:

  * Linux (GNU + musl)
  * macOS (Intel + Apple Silicon)
  * Windows (MSVC + GNU)
  * Android ARM64

---

## Behavior Changes

* Zero / Discard operations may now be skipped when proven redundant
* Progress reporting now measures bytes written instead of operations
* Cleanup logic is more aggressive and deterministic on failure

---

## Code Quality & Maintainability

* Clearer separation between hot paths and generic logic
* Reduced unsafe surface area in performance-critical code
* Better internal documentation of invariants
* More predictable control flow in concurrent extraction
* Easier future SIMD and architecture extensions

---

## Fixed

* Cache-thrashing behavior on very large partitions
* Subtle SIMD zero-check correctness issues
* Excess synchronization in parallel extraction
* Progress bar inaccuracies on mixed operation sizes
* Rare cancellation edge cases under heavy load

---

## Notes

This release focuses on **making v2.0 faster, leaner, and more scalable** rather than adding flashy features.
Most improvements are invisible to users — except in **dramatically improved throughput** on large OTAs.

---

## Credits 

Thanks to all testers and contributors. Special appreciation to everyone who tested real OTAs at scale and helped refine performance, correctness, and reliability.

---

