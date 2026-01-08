# Technical Documentation

This document provides detailed technical information about otaripper's architecture, design decisions, and implementation details.

## Table of Contents

* [Architecture Overview](#architecture-overview)
* [Memory Management](#memory-management)
* [SIMD Optimization](#simd-optimization)
* [Verification Pipeline](#verification-pipeline)
* [Parallel Extraction](#parallel-extraction)
* [Reliability and Failure Handling](#reliability-and-failure-handling)
* [Performance Architecture](#performance-architecture)
* [Advanced Configuration](#advanced-configuration)

---

## Architecture Overview

otaripper is structured around three core principles:

1. **Safety First**: Memory safety through Rust's type system, comprehensive verification
2. **Zero-Copy I/O**: Memory-mapped file operations to minimize overhead
3. **Contention-Free Concurrency**: Parallel extraction with workers operating on disjoint regions

### Key Components

* **Payload Parser**: Parses Android OTA manifest and payload structure
* **Memory Mapper**: Manages memory-mapped I/O for input and output files
* **Worker Pool**: Executes extraction operations in parallel
* **Verification Engine**: SHA-256 validation and sanity checking
* **Progress Monitor**: Lock-free progress tracking

---

## Memory Management

### Zero-Copy Memory Mapping

otaripper uses memory-mapped I/O to avoid unnecessary data copying:

```
Traditional Approach:
  File → read() → Kernel Buffer → User Buffer → Process
  (Multiple copies, high overhead)

otaripper Approach:
  File → mmap() → Direct Process Access
  (Zero-copy, minimal overhead)
```

**Benefits:**

* **60-80% fewer memory copies**: Direct access to file data
* **Lower RAM usage**: Pages loaded on-demand by the OS
* **OS-level caching**: Kernel manages page cache automatically
* **Safe concurrent reads**: Read-only mappings prevent data races

**Implementation Details:**

* Input payload: Read-only memory mapping
* Output partitions: Write-only memory mapping with pre-allocation
* Extent validation: Ensures non-overlapping memory regions at startup
* Page-aligned operations: 4KB boundaries for optimal performance

### Memory Layout

```
┌─────────────────────────────────────┐
│   Input: payload.bin (mmap)         │
│   - Read-only mapping               │
│   - Shared across all workers       │
│   - Kernel manages paging           │
└─────────────────────────────────────┘
             ↓
┌─────────────────────────────────────┐
│   Worker Threads (N parallel)       │
│   - Each processes disjoint extents │
│   - No locking in the hot path
│   - Independent decompression       │
└─────────────────────────────────────┘
             ↓
┌─────────────────────────────────────┐
│   Output: partition.img (mmap)      │
│   - Write-only mapping              │
│   - Pre-allocated to full size      │
│   - Non-overlapping write regions   │
└─────────────────────────────────────┘
```

---

## SIMD Optimization

### Automatic CPU Detection

otaripper detects CPU capabilities at startup and selects the optimal code path:

```rust
CPU Feature Detection:
┌─────────────────────────────────────┐
│ 1. Check AVX-512F + AVX-512BW       │
│    ├─ Available? → 512-bit SIMD     │ (8x throughput)
│    └─ Not available → Check AVX2    │
│                                     │
│ 2. Check AVX2                       │
│    ├─ Available? → 256-bit SIMD     │ (4x throughput)
│    └─ Not available → Check SSE2    │
│                                     │
│ 3. Check SSE2                       │
│    ├─ Available? → 128-bit SIMD     │ (2x throughput)
│    └─ Not available → Scalar        │
│                                     │
│ 4. Fallback: Scalar operations      │ (1x baseline)
└─────────────────────────────────────┘
```

### SIMD Applications

**1. Memory Operations**
* SIMD-accelerated large block copying (SSE2/AVX2/AVX-512)
* Streaming non-temporal writes (`_mm*_stream_si*`)
* Cache-friendly chunking for optimal performance
* Processes 16-64 bytes per instruction depending on CPU

**2. All-Zero Detection (Sanity Checking)**
* SIMD-accelerated corruption detection
* Processes 16-64 bytes per instruction
* Near-zero overhead for safety checks
* Catches obviously invalid partition images

**3. SHA-256 Verification**

otaripper uses the `sha2` Rust crate for SHA-256 verification. This is a well-maintained, standards-compliant, and constant-time implementation.

Hashing is not SIMD-parallelized. Performance is generally dominated by I/O and decompression, so SHA-256 rarely becomes the bottleneck in practice. On NVMe SSDs, hashing typically still runs near storage speed, so verification does not meaningfully slow extraction.

### Debug CPU Detection

```bash
# See detected CPU features
OTARIPPER_DEBUG_CPU=1 ./otaripper ota.zip

# Example output:
# CPU Features detected:
#   AVX-512: Yes
#   AVX2: Yes
#   SSE2: Yes
# Using: AVX-512 optimized path
```

---

## Verification Pipeline

otaripper implements a three-layer verification system:

### Layer 1: Input Verification (Always Enabled)

```
┌─────────────────────────────────────┐
│ 1. Parse payload.bin manifest       │
│    - Verify protobuf structure      │
│    - Validate partition metadata    │
│    - Check extent boundaries        │
│                                     │
│ 2. Validate manifest hashes         │
│    - Verify hash presence           │
│    - Check hash format (SHA-256)    │
│    - Validate extent checksums      │
└─────────────────────────────────────┘
```

**Purpose**: Catch corrupted or malformed input files before extraction begins.

### Layer 2: Operation Verification (Default: Enabled)

```
┌─────────────────────────────────────┐
│ For each operation:                 │
│   1. Read compressed extent data    │
│   2. Verify data hash (if present)  │
│   3. Decompress (bzip2/xz/raw)      │
│   4. Write to output partition      │
│      - SIMD-accelerated copy        │
│      - Streaming writes for large   │
│        blocks (cache bypass)        │
└─────────────────────────────────────┘
```

**Purpose**: Ensure data integrity during extraction. Disabled with `--no-verify`.

### Layer 3: Output Verification (Default: Enabled)

```
┌─────────────────────────────────────┐
│ After extraction completes:         │
│   1. Compute SHA-256 of output file │
│   2. Compare with manifest hash     │
│   3. Perform sanity checks (optional)│
│      - All-zero detection           │
│      - Size validation              │
│   4. Report verification status     │
└─────────────────────────────────────┘
```

**Purpose**: Verify final output integrity. Enforced with `--strict`.

### Verification Modes

| Mode | Layer 1 | Layer 2 | Layer 3 | Use Case |
|------|---------|---------|---------|----------|
| Default | ✅ | ✅ | ✅ | Standard extraction |
| `--strict` | ✅ | ✅ | ✅ (enforced) | Maximum safety |
| `--no-verify` | ✅ | ❌ | ❌ | Trusted sources only |
| `--sanity` | ✅ | ✅ | ✅ + extra checks | Forensic analysis |

---

## Parallel Extraction

### Contention-Free Worker Design

otaripper uses a contention-free architecture for maximum concurrency:

```
Main Thread:
  ├─ Parse manifest
  ├─ Validate extents (ensure no overlap)
  ├─ Create memory-mapped output files
  ├─ Spawn worker threads
  └─ Wait for completion

Worker Thread (per operation):
  ├─ Read compressed data from payload (read-only mmap)
  ├─ Decompress in thread-local buffer
  ├─ Write to partition file (write-only mmap, disjoint regions)
  ├─ Update progress (atomic counter)
  └─ Signal completion (lockless queue)
```

### Concurrency Guarantees

**No Contention in Hot Path Because:**

1. **Disjoint Memory Regions**: Each worker writes to non-overlapping extents
2. **Read-Only Input**: All workers share read-only payload mapping
3. **Atomic Progress Updates**: Lock-free counters for progress tracking
4. **Independent Decompression**: Each worker has private decompressor state

Workers operate on disjoint regions, avoiding shared locks in the hot path. Synchronization only occurs at partition completion for final verification.

**Validation at Startup:**

```rust
// Pseudo-code for extent validation
for each operation:
    for each other_operation:
        if ranges_overlap(operation, other_operation):
            panic!("Invalid manifest: overlapping extents")
```

### Thread Pool Configuration

**Auto-Detection (Default)**:
* Uses `num_cpus::get()` to detect logical cores
* Optimal for most systems

**Manual Override**:
```bash
# Use 8 worker threads
./otaripper ota.zip -t 8

# Use 1 thread (sequential)
./otaripper ota.zip -t 1

# Use 32 threads (high-core systems)
./otaripper ota.zip -t 32
```

**Performance Notes**:
* Benefits diminish beyond ~16 threads on most systems (I/O bound)
* SSD storage scales better with higher thread counts
* HDD storage may perform worse with excessive threads (seek overhead)

---

## Reliability and Failure Handling

### Error Handling Philosophy

otaripper follows a "fail-fast, clean-up always" approach:

1. **Detect errors early**: Validate before extraction begins
2. **Stop immediately**: Don't continue with invalid state
3. **Clean up completely**: Remove partial files
4. **Report clearly**: Provide actionable error messages

### Failure Scenarios

**Scenario 1: Verification Failure**
```
Event: Output hash doesn't match manifest
Action:
  1. Stop all workers
  2. Delete corrupted partition file
  3. Report which partition failed
  4. Exit with error code
```

**Scenario 2: Interrupted Extraction (Ctrl+C)**
```
Event: User presses Ctrl+C
Action:
  1. Catch SIGINT signal
  2. Signal all workers to stop
  3. Wait for workers to finish current operation
  4. Delete ALL partition files created (including successful ones)
  5. Delete output directory if otaripper created it
  6. Print cleanup status
  7. Exit gracefully
```

**Scenario 3: Disk Full**
```
Event: Write operation fails (ENOSPC)
Action:
  1. Stop all workers immediately
  2. Delete ALL partition files created
  3. Delete output directory if otaripper created it
  4. Report disk space issue
  5. Exit with error code
```

**Scenario 4: Invalid Manifest**
```
Event: Extent overlap detected
Action:
  1. Fail before any extraction begins
  2. Report manifest validation error
  3. Exit immediately (no cleanup needed)
```

### Cleanup Guarantees

otaripper guarantees **all-or-nothing extraction safety**. If extraction fails, nothing risky is left behind.

**If extraction is interrupted or fails:**
* ✅ All created partition images are deleted (even successfully extracted ones)
* ✅ Temporary files and memory mappings are cleaned
* ✅ Output directory is removed if otaripper created it
* ✅ No "half-good / half-corrupt" situation possible

**If extraction succeeds:**
* ✅ Everything remains intact
* ✅ Folder auto-opens (unless `-n`)

**Edge case: Existing output directory**
* If directory already existed before extraction:
  * All created files are still deleted on failure
  * Directory itself is kept (since user created it)
* If otaripper created the directory:
  * Entire directory is removed on failure

---

## Performance Architecture

### Bottleneck Analysis

otaripper performance is typically bounded by:

1. **Storage I/O** (most common bottleneck)
   * Read speed of input OTA file
   * Write speed of output partition files
   * Sequential vs random access patterns

2. **Decompression** (for heavily compressed OTAs)
   * bzip2: CPU-intensive, slower
   * xz/lzma: CPU-intensive, slower
   * uncompressed: No overhead

3. **CPU** (least common bottleneck)
   * Hash computation (SHA-256)
   * SIMD optimization helps significantly

### Optimization Strategies

**1. Memory-Mapped I/O**
* Eliminates user-kernel copies
* Leverages OS page cache
* Reduces context switches

**2. Contention-Free Design**
* Workers operate on disjoint memory regions
* No shared locks in the hot path
* Scales linearly with cores (up to I/O limit)

**3. SIMD Acceleration**
* Accelerated memory copying (SSE2 / AVX2 / AVX-512)
* Non-temporal streaming writes for very large blocks
* SIMD-accelerated zero detection
* Reduces CPU overhead during extraction

**4. Extent Coalescing**
* Groups contiguous extents
* Reduces number of operations
* Better cache locality

### Performance Measurement

**Built-in Statistics** (`--stats`):
```
Extraction statistics:
  - boot: 64.0 MB in 45 ms (1.42 GB/s)
  - vendor_boot: 128.0 MB in 67 ms (1.91 GB/s)
  - system: 2.1 GB in 1205 ms (1.74 GB/s)
  Total: 2.29 GB in 1317 ms (1.74 GB/s)
```

**Interpretation**:
* **< 500 MB/s**: Likely HDD-bound or compressed data
* **500-1500 MB/s**: SATA SSD or moderately compressed
* **> 1500 MB/s**: NVMe SSD with uncompressed data

---

## Advanced Configuration

### Environment Variables

**`OTARIPPER_DEBUG_CPU`**
```bash
OTARIPPER_DEBUG_CPU=1 ./otaripper ota.zip
```
Enables CPU feature detection logging.

### Build-Time Optimizations

**Target CPU Features** (requires nightly Rust):
```bash
# Enable all CPU features available on build machine
RUSTFLAGS="-C target-cpu=native" cargo build --release

# Optimize for specific CPU
RUSTFLAGS="-C target-cpu=haswell" cargo build --release
```

**Link-Time Optimization**:
```toml
# Add to Cargo.toml
[profile.release]
lto = "fat"
codegen-units = 1
```

**Platform-Specific Tuning**:

Linux:
```bash
# Use mold linker for faster builds
mold -run cargo build --release
```

Windows:
```bash
# Use MSVC with PGO (Profile-Guided Optimization)
cargo pgo build
```

---

## Design Decisions

### Why Memory-Mapped I/O?

**Advantages**:
* Zero-copy operations
* OS manages caching
* Simplified code (no manual buffering)

**Disadvantages**:
* Less portable (works well on modern OSes)
* Can't use async I/O easily
* Requires careful extent validation

**Decision**: Benefits outweigh drawbacks for this use case.

### Why Contention-Free Design?

**Advantages**:
* Better scalability
* No lock contention
* Simpler reasoning about correctness

**Disadvantages**:
* Requires careful validation
* More complex error handling

**Decision**: Android OTA structure (disjoint extents) makes this safe and beneficial.

### Why Rust?

**Key Benefits**:
* Memory safety without garbage collection
* Zero-cost abstractions
* Excellent tooling and ecosystem
* Strong type system prevents common bugs

**Trade-offs**:
* Steeper learning curve
* Longer compile times

**Decision**: Safety and performance requirements justify the choice.

---

## Future Optimizations

### Planned Improvements

1. **Incremental OTA support**
   * Apply delta patches
   * Requires base image handling

2. **Graphical User Interface (GUI)**
   * Enhanced ease of use: Provides an intuitive visual layout that simplifies complex workflows for end-users.
   * Reduced technical barrier: Enables non-technical users to manage updates and system settings without using command-line tools.
   
### Performance Goals

* **NVMe saturation**: 3+ GB/s on high-end drives
* **Lower memory footprint**: < 50 MB for typical OTAs
* **Faster hash computation**: Explore hardware SHA extensions

---

## Troubleshooting

### Performance Issues

**Symptom**: Slower than expected extraction

**Possible Causes**:
1. HDD storage (seek overhead)
2. Highly compressed OTA (CPU bound)
3. Excessive thread count (diminishing returns)
4. Background processes competing for I/O

**Solutions**:
* Use SSD for input/output
* Reduce thread count: `-t 8`
* Check with `--stats` to identify bottleneck

### Verification Failures

**Symptom**: Hash mismatch errors

**Possible Causes**:
1. Corrupted input OTA file
2. Disk errors during extraction
3. RAM corruption (rare)

**Solutions**:
* Re-download OTA file
* Run disk diagnostics
* Use `--strict` mode to catch issues early

---

## References

* [Android OTA Format Documentation](https://source.android.com/devices/tech/ota)
* [Memory-Mapped I/O (mmap)](https://man7.org/linux/man-pages/man2/mmap.2.html)
* [SIMD Intrinsics Guide](https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html)
* [Rust Book](https://doc.rust-lang.org/book/)

---

For user-facing documentation, see [README.md](README.md)
