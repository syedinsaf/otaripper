# Technical Documentation

This document provides detailed technical information about **otaripper’s** architecture, design decisions, and implementation details.

> **v2.2 Note:**
> This release introduces significant architectural scalability and performance refinement.
> The monolithic execution logic has been decoupled into specialized modules. Furthermore, memory allocation overhead has been significantly reduced via thread-local buffer pooling, enabling purely alloc-free zero-copy decompression paths.

---

## Table of Contents

* [Architecture Overview](#architecture-overview)
* [Memory Management](#memory-management)
* [SIMD Optimization](#simd-optimization)
* [Verification Pipeline](#verification-pipeline)
* [Parallel Extraction](#parallel-extraction)
* [Reliability and Failure Handling](#reliability-and-failure-handling)
* [Performance Architecture](#performance-architecture)
* [Advanced Configuration](#advanced-configuration)
* [Design Decisions](#design-decisions)
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

### Zero-Copy Memory Mapping

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

1. Memory-mapped I/O
2. Contention-free parallelism
3. SIMD acceleration
4. Extent coalescing
5. **Hot-path specialization (v2.1)**

---

### Built-in Statistics (`--stats`)

Reports per-partition and total throughput to identify bottlenecks.

---

## Advanced Configuration

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
