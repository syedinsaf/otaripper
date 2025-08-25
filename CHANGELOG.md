# OTARipper v1.2.0 Complete Changelog

## Download Instructions

**Windows** → `otaripper-windows-msvc.exe` (fallback: `otaripper-windows-gnu.exe`)  
**Linux** → `otaripper-linux-gnu` (fallback: `otaripper-linux-musl`)  
**macOS** → `otaripper-macos`

*Linux/macOS users: Run `chmod +x otaripper-*` after download*

---

## New Command Line Flags

- `--strict` - Require cryptographic hashes for all partitions and operations; fail if any required hash is missing
- `--print-hash` - Compute and print SHA-256 of each extracted partition image
- `--plausibility-checks` - Run quick sanity checks on output images (e.g., detect all-zero images)
- `--stats` - Print per-partition and total timing/throughput statistics after extraction
- `--no-open-folder` - Don't automatically open the extracted folder after completion

## Performance Improvements

### SIMD Acceleration
- Automatic CPU feature detection (AVX512, AVX2, SSE2)
- 2-10x faster data copying operations using vectorized instructions
- Optimized zero-detection for plausibility checks
- Fallback to scalar operations on unsupported hardware

### Memory Operations
- Cache-friendly chunked processing with 64KB optimal chunk size
- Intelligent extent coalescing to merge adjacent memory regions
- Reduced copy operations through better memory layout
- SIMD threshold of 1KB for optimal performance switching

### Smart Hashing
- Inline SHA-256 computation for large partitions (>256MB threshold)
- Avoids separate hash passes on large files
- Environment variable `OTARIPPER_INLINE` for manual control (on/off/auto)
- Reuses computed hashes for verification to eliminate redundant passes

### Threading Improvements
- Enhanced thread pool management with automatic CPU core detection
- Reduced contention between worker threads
- Improved progress bar update frequency (adaptive based on partition count)
- Better thread count validation (1-256 range)

## Safety and Reliability

### Error Handling
- Graceful Ctrl+C handling with proper cleanup of partial files
- Automatic removal of corrupted partial files on extraction failure
- Comprehensive panic handling with cleanup hooks
- Cancellation tokens to stop worker threads on critical errors

### Memory Safety
- Replaced all raw pointer operations with safe slice handling
- Proper Rust lifetime management throughout codebase
- Enhanced bounds checking for extent validation
- Safe mutable slice splitting for non-overlapping regions

### Verification Enhancements
- Strict mode requiring all hashes to be present
- Better hash mismatch error messages with hex encoding
- Plausibility checks to detect obviously invalid outputs
- Optional verification bypass with `--no-verify` (conflicts with `--strict`)

## User Experience

### Interface Improvements
- Friendlier help template with task-oriented examples
- Automatic opening of extracted folder after completion
- Total extracted folder size calculation and display
- Enhanced progress bars with adaptive refresh rates
- Colored output with consistent styling

### Better Error Messages
- More informative error reporting with context
- Clear platform-specific download guidance
- Helpful usage examples in help text
- Thread count validation with helpful suggestions

### Output Management
- Timestamped output directories when using `-o` flag
- Clean hash printing after extraction completion
- Statistics summary with throughput calculations
- Directory cleanup on user interruption

## Technical Architecture

### Payload Handling
- `PayloadSource` enum for unified memory-mapped vs. in-memory handling
- In-memory ZIP extraction instead of temporary files
- Better decompression support for BZ2 and XZ formats
- Enhanced payload parsing with proper error context

### Operation Support
- Enhanced REPLACE operation with inline hashing
- Proper REPLACE_BZ and REPLACE_XZ decompression
- ZERO and DISCARD operation support
- Better handling of unsupported operation types with clear error messages

### Data Structures
- `ExtentsWriter` for efficient writing across multiple memory regions
- Optional on-the-fly hashing during write operations
- Coalesced extent processing to reduce fragmentation
- Safe extent extraction with lifetime guarantees

### Concurrency
- Channel-based communication for statistics and hash collection
- Reduced lock contention through better data structure design
- Scoped thread management ensuring memory safety
- Atomic operations for progress tracking and cancellation

## Bug Fixes

### Memory Issues
- Fixed memory alignment issues in block operations
- Resolved potential buffer overflow in extent handling
- Corrected slice bounds checking
- Fixed race conditions in multi-threaded hash computation

### Operation Handling
- Better validation of operation data lengths
- Improved EOF detection in compressed streams
- Fixed block alignment calculations
- Enhanced data offset validation

### File System
- Proper cleanup of temporary files and directories
- Better handling of file creation conflicts
- Improved cross-platform folder opening
- Fixed directory size calculation edge cases

## Code Quality

### Architecture Improvements
- Modular SIMD implementations with feature gates
- Clean separation of concerns between parsing and extraction
- Better error propagation with anyhow context
- Consistent naming and documentation

### Testing and Validation
- Environment variable for SIMD debugging (`OTARIPPER_DEBUG_CPU`)
- Better validation of thread pool configuration
- Enhanced bounds checking throughout
- Comprehensive error path testing

### Platform Support
- Conditional compilation for x86_64 SIMD features
- Fallback implementations for non-x86_64 architectures
- Cross-platform folder opening support
- Better handling of different filesystem capabilities

---

## Migration from v1.x

Most command-line usage remains the same. New features are opt-in via flags:

```bash
# v1 behavior (still works)
otaripper ota.zip

# v2 enhanced usage
otaripper ota.zip --print-hash --stats --plausibility-checks
```

The `--strict` flag provides additional safety but requires all hashes to be present in the manifest.