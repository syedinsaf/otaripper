# Changelog – **otaripper v2.0.0**

### Major Release

This update is a **major rewrite and stability-focused upgrade** with significant improvements in performance, safety, correctness, CLI user experience, and multi-platform build reliability.

---

## New Features

* Android ARM64 build support
* macOS ARM64 + Intel builds
* Linux musl static build
* Windows GNU + MSVC builds
* Timestamped structured output folders
* Automatically open extracted folder (optional)
* Human-explainer error messages
* Improved `--list` mode with Full / Incremental indication
* Incremental OTA detection with safe refusal
* Smart inline hashing (with fallback)
* SIMD streaming mode (AVX512 / AVX2 non-temporal stores)
* Adaptive payload.bin memory handling (RAM-aware)
* Ctrl+C safe cancellation with automatic cleanup

---

## Performance Improvements

* Fully rewritten extraction engine
* New SIMD pipeline:

  * AVX-512 streaming and normal modes
  * AVX2 streaming and normal modes
  * SSE2 fallback
* Optimized extent coalescing to reduce write operations
* Improved threading model
* Reduced allocation overhead
* Inline hashing for large partitions when possible
* Faster SHA-256 verification
* Runtime CPU SIMD capability detection

---

## Reliability & Safety

* Non-overlapping extent validator
* Safer slice ownership model — eliminates UB risks
* Stronger block size checks
* Safer payload bounds validation
* Manifest integrity validation
* Safer memory mapping model
* Guaranteed cleanup on failure, crash, or interruption
* Sanity checking (`--sanity`) detects clearly invalid partitions

---

## UX / CLI Improvements

* New, clearer help template
* Real usage guidance and examples
* Actionable error messages
* Improved progress display
* Ordered and clean SHA output when printing hashes
* Better thread configuration UX
* Disable automatic folder opening using `-n` / `--no-open`

---

## Build System Upgrades

* Reworked CI workflow
* Automated multi-platform builds
* Binary stripping enabled
* Tuned Cargo and Rust flags
* Safer Windows static linking logic
* Android NDK build pipeline
* Updated protobuf build flow
* Multiple dependency upgrades
* Default MiMalloc global allocator

---

## Behavior Changes

* Incremental OTA files are now safely rejected
* `--plausibility-checks` renamed to `--sanity`
* `--no-open-folder` renamed to `--no-open` (or simply -n)
* Removed unsafe mmap write behavior
* Improved strict mode behavior
* Replaced legacy unsafe code paths with validated logic

---

## Code Quality

* Major refactor for correctness and safety
* Significantly reduced unsafe usage
* Clearer module structure
* Stronger internal documentation
* Better separation of responsibilities

---

## Fixed

* Manifest edge case parsing errors
* Risky extent behavior
* Concurrency issues in mmap handling
* Ctrl+C cleanup race
* AVX large block handling bugs
* SSE2 mask logic bug
* Windows hash mismatch paths
* Output folder cleanup reliability

---

## Credits

Thanks to all testers and contributors. Special appreciation to everyone who stress-tested real OTAs and helped sharpen correctness and reliability.

---
