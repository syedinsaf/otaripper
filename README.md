<!-- markdownlint-configure-file {
  "MD033": false,
  "MD041": false
} -->

<div align="center">

# otaripper

**`otaripper` helps you extract partitions from Android OTA files.** <br />
Partitions can be individually flashed to your device using `fastboot flash`.

Compared to other tools, `otaripper` is significantly faster and handles file
verification - no fear of a bad OTA file bricking your device.

</div>

## Features

|                              | [syedinsaf/otaripper] | [ssut/payload-dumper-go] | [vm03/payload_dumper]                     |
| ---------------------------- | --------------------- | ------------------------ | ----------------------------------------- |
| Input file verification      | ✔                     | ✔                        |                                           |
| Output file verification     | ✔                     |                          |                                           |
| Extract selective partitions | ✔                     | ✔                        | ✔                                         |
| Parallelized extraction      | ✔                     | ✔                        |                                           |
| Runs directly on .zip files  | ✔                     | ✔                        |                                           |
| Incremental OTA support      |                       |                          | [Partial][payload_dumper-incremental-ota] |



## Installation

### macOS / Linux

Install a pre-built binary:

```sh
curl -sS https://raw.githubusercontent.com/syedinsaf/otaripper/main/install.sh | bash
```
Or download the pre-built binary from the [Releases] page. 

### Windows

1. Download the pre-built binary from the [Releases] page. 
2. Extract it. 
3. Have the payload.bin or the ota.zip in the same extracted folder as otaripper.exe. 

## Usage

Run the following command in your terminal:

```sh
# Run directly on .zip file.
otaripper ota.zip (on Windows)
./otaripper ota.zip (on Linux)

# Run on payload.bin file.
otaripper payload.bin (on Windows)
./otaripper ota.bin (on Linux)

```
## To extract your desired Partitions add "--partitions" and then your desired parition.

```sh
# For example, if you want to extract just the boot image, you can do this:
./otaripper  payload.bin --partitions boot

# If you want multiple desired images, you can separate them by a ","
./otaripper  payload.bin --partitions boot,init_boot
```
## Contributors

- [Syed Insaf][syedinsaf]

[syedinsaf]: https://github.com/syedinsaf
[payload_dumper-incremental-ota]: https://github.com/vm03/payload_dumper/issues/53
[releases]: https://github.com/syedinsaf/otaripper/releases
[syedinsaf/otaripper]: https://github.com/syedinsaf/otaripper
[ssut/payload-dumper-go]: https://github.com/ssut/payload-dumper-go
[vm03/payload_dumper]: https://github.com/vm03/payload_dumper


## Building from Source

### Prerequisites
- Rust 1.82.0 or later
- Cargo (comes with Rust)

### Build Commands

#### Standard Build (Recommended)
```sh
# Clone the repository
git clone https://github.com/syedinsaf/otaripper.git
cd otaripper

# Build optimized release binary
cargo build --release
```

#### Platform-Specific Optimizations

**Linux/macOS (with assembly optimizations):**
```sh
cargo build --release --features asm-sha2
```

**Windows (compatibility build):**
```sh
cargo build --release
```

**Using build scripts:**
```sh
# Linux/macOS
./build.sh

# Windows
build.bat
```

### Build Features

- `asm-sha2`: Enables assembly-optimized SHA-256 hashing for better performance on Unix-like systems
  - **Note**: This feature is not compatible with Windows MSVC and will cause build failures
  - Automatically disabled on Windows for compatibility

### Build Output
The compiled binary will be available at:
- Linux/macOS: `target/release/otaripper`
- Windows: `target/release/otaripper.exe`

## Advanced integrity flags

- --strict
  - Enforces cryptographic verification strictly.
  - Fails immediately if any selected partition is missing a manifest hash or if any operation that carries data is missing its data_sha256_hash.
  - Implies verification is enabled; cannot be combined with --no-verify.
  - On any failure, the run is aborted and any partially extracted images are deleted.

- --print-hash
  - After each partition is fully extracted, computes and prints the SHA-256 of the resulting image (e.g., system: sha256=...).
  - If the manifest already provides a partition hash, verification still occurs as usual; the printed value allows you to record or compare hashes easily.
  - Performance: may add one linear pass over the image when the manifest lacks a partition hash.

- --plausibility-checks
  - Runs quick, non-cryptographic sanity checks on the final image. Currently rejects images that are entirely zeros.
  - Useful to catch obviously invalid outputs even when hashes are unavailable.
  - Overhead is negligible (only scans small chunks across the image).

### Existing flags (for reference)
- -p, --path PATH or positional PATH: select the OTA .zip or payload.bin.
- -o, --output-dir PATH: where to write extracted partitions.
- -t, --threads NUMBER: limit worker threads. When set, otaripper prints the effective number of worker threads being used.
- --partitions a,b,c: extract only the listed partitions.
- --no-verify: skip all hash verification (dangerous). Do not combine with --strict.
- -l, --list: list partitions in the payload instead of extracting.

### Error handling and cleanup
- If any error occurs (logic error, verification failure, corrupted input), otaripper stops immediately and deletes any partially created partition images. It may also remove the output directory if it was created by this run.

## Performance and stats
- --stats
  - Prints per-partition and total extraction time and throughput after completion.
  - Disabled by default; has no overhead unless enabled.

## Extraction completion information
- **Folder size display**: After successful extraction, otaripper automatically displays:
  - Confirmation of successful extraction
  - Output directory path
  - Total size of all extracted files in human-readable format
- This feature helps users verify the extraction was complete and understand the storage requirements

- **Automatic folder opening**: After successful extraction, otaripper automatically opens the extracted folder in your default file manager
- Use `--no-open-folder` to disable this behavior if you prefer to open the folder manually

## CPU Compatibility and Optimizations

### CPU Compatibility
- otaripper is optimized for modern CPUs with AVX, AVX2, and AVX512 instruction sets
- Runtime detection automatically chooses the best available instruction set
- Compatible with Intel Haswell and newer CPUs, as well as equivalent AMD processors
- AVX512 support provides the best performance on supported CPUs

### Compiler and Runtime Optimizations
- Link Time Optimization (LTO) for better cross-module optimizations
- Maximum optimization level (opt-level = 3)
- Panic abort strategy for smaller binary size
- Disabled integer overflow checks for better performance
- Single codegen unit for better optimization opportunities
- SIMD-accelerated operations for performance-critical functions
- Assembly-optimized SHA-256 hashing for faster verification
- Native LZMA implementation for improved decompression speed
- Vectorized zero-detection for efficient integrity checks
