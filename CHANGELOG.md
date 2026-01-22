# Changelog – **otaripper v2.1.0**

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

