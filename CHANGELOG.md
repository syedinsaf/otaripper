# Changelog

## **otaripper v3.2.1** (2026-05-26)

### Smart Firmware Metadata & UX Refinements

This release significantly enhances the user experience by automatically parsing firmware metadata to enrich CLI outputs, extraction folder names, and ARB JSON exports, while also streamlining CLI logging and silencing notorious Linux GUI errors.

* **Firmware Information Banner**
  * Automatically parses `META-INF/com/android/metadata` and `payload_properties.txt` from OTA packages before extraction begins.
  * Prints a beautiful, clean "Firmware Information" banner detailing Product Model, Android Version, Build Version, OxygenOS/ColorOS Version, and Security Patch level.
* **Streamlined Command Line Output**
  * Simplified the `arb` (arbscan) subcommand output: displays only the filename being analyzed (e.g., `Analyzing: xbl_config.img`) and hides long paths or remote CDN URL parameters.
  * Cleaned up the main firmware information display to hide redundant or confusing internal target versions and update type fields.
* **Dynamic Output Folder Naming**
  * The extraction output directory is now dynamically named using the actual OS build version string if found (e.g., `extracted_CPH2573_15.0.0.860(EX01)_2026-05-26...`) instead of just a generic timestamp.
* **Intelligent ARB Auto-Detection**
  * The `arb` subcommand automatically inherits the firmware metadata parsing logic.
  * It auto-detects the device model and OS version, securely skipping the manual user prompts and directly formatting the JSON output filename (e.g., `CPH2573_15.0.0.860(EX01)_ARB(0).json`).
* **Silenced Qt Terminal Spam**
  * Explicitly redirects `stdout` and `stderr` to `/dev/null` when spawning GUI file managers (like Dolphin) post-extraction, completely silencing the notorious `QPixmap::scaled: Pixmap is a null pixmap` Linux terminal spam.
* **Fastboot Firmware Flasher Integration**
  * Added the `--fff` CLI flag to automatically reorganize extracted partition images into standard subdirectories (`BOOTLOADER`, `CRITICAL`, `MODEM`, `SYSTEM`, `EXTRA`) nested inside a device-named folder (e.g., `CPH2573_15.0.0.860(EX01)`), enabling direct compatibility with firmware flashing scripts.
  * Added support for reorganizing previously extracted output directories (e.g., `otaripper <dir_path> --fff`), which automatically moves all `.img` files into a clean device sibling directory structure and prompts the user before deleting the original empty folder.

---

## **otaripper v3.1.1** (2026-05-17)

### Modern Qualcomm ARB & Supercharged JSON UX

This release introduces a critical validation update to support modern Snapdragon bootloaders alongside a highly polished, robust, and interactive JSON metadata export workflow.

* **Support for Modern Qualcomm SoCs (Snapdragon 8 Gen 1/2/3)**
  * Updated [`find_hash_header`](file:///e:/Downloads/Music/otaripper/src/cmd/arbscan.rs#L109-L145) to accept hash table sizes that are a multiple of 16 (using mask `& 0xF`) instead of strictly requiring a multiple of 32 (using mask `& 0x1F`).
  * This successfully resolves scanning failures on newer Snapdragon devices utilizing **SHA-384** (48-byte digests) for Secure Boot validation, while remaining compatible with older **SHA-256** bootloaders and future-proofing the engine for potential **SHA-512** (64-byte digests) implementations.

* **Robust URL & Signed URL Parsing**
  * Implemented an advanced, URL-aware and platform-independent file stem parser that cleanly strips out complex query parameters, CDN expiry tags, and schemes, sanitizing all remaining filesystem-unfriendly characters to prevent crashes on Windows during JSON output writes.

* **Descriptive Prompt-Driven JSON Filenames**
  * When exporting analyzed ARB metadata, the CLI now dynamically names the output JSON file using the user-supplied "Device model" and "Update / build" fields (e.g. `op12_10.1.100_ARB(0).json` instead of a vague CDN hash stem like `63ad71bd5de042d7aaff5b01496dc12b_arb.json`), making files instantly recognizable at a glance.

* **Polished Input Cleansing & Suffix Formatting**
  * Pre-trims spaces, slashes, and backslashes from user prompts (like `16.0.7.500\`) to ensure clean filenames without duplicate underscores.
  * Automatically appends the actual Anti-Rollback (ARB) index level as an `_ARB(N).json` suffix to both custom inputs and fallback file stems.

---

## **otaripper v3.1.0** (2026-05-17)

### Parallel Chunked Streaming & Network Resilience

This release focuses on hardening **Remote HTTP Extraction** for unreliable network environments and saturating high-bandwidth connections. It introduces parallel chunked requests, a dedicated bandwidth monitor, and a pure-native networking stack optimized for Android.

---

## Parallel HTTP Streaming

* **Multi-Threaded Chunking**
  * Large remote partitions are now split into multiple **8MB parallel Range requests**.
  * This allows `otaripper` to fully saturate high-bandwidth connections that might otherwise be throttled per-connection by CDNs.
* **Global Bandwidth Monitor**
  * Added a persistent **"Network:" progress bar** at the bottom of the multi-progress view.
  * Tracks aggregate download progress, real-time speed, and total data retrieved across all worker threads.

---

## Network Resilience & UX

* **Robust Connection Recovery**
  * Implemented an **infinite retry loop with exponential backoff** (500ms to 3s).
  * If a connection is dropped mid-stream, `otaripper` now waits and resumes exactly where it left off instead of failing the extraction.
* **Dynamic Offline UI**
  * The UI now detects connection losses in real-time, displaying a **"connection lost, waiting to resume..."** status until the network is restored.
* **Smart Rate Limiting**
  * Explicitly handles **HTTP 429 (Too Many Requests)** by automatically cooling down and retrying, preventing bans from aggressive CDNs.

---

## Infrastructure & Security

* **Pure-Native Android Support**
  * Pinned `reqwest` to ensure a completely native TLS/networking stack. This eliminates dependencies on Java-based platform verifiers that caused panics in Android terminal environments (Termux/ADB).
* **Advanced DNS Resolver Selection**
  * **Linux/Windows**: Continues using `hickory-dns` to bypass `musl libc` cold-connection bugs.
  * **Android**: Now dynamically detects the environment and falls back to the **Native OS Resolver** to handle broken `/etc/resolv.conf` symlinks on custom ROMs.
* **Security Hardening**
  * Implemented a **256MB manifest size limit** to prevent OOM attacks from malicious remote servers.
  * Added validation to ensure remote servers correctly support **HTTP Range requests** before starting extraction.

---

## **otaripper v3.0.0** (2026-05-15)

### Flawless Remote HTTP Streaming & Release Infrastructure

This major release perfects **Remote HTTP Extraction**. You can now extract specific partitions directly from a remote URL over the internet, completely bypassing the need to download massive 3GB+ OTA zip files. It also heavily hardens networking reliability across OS environments and overhauls release artifact verification.

---

## Remote Web Streaming & OS Compatibility

* **Zero-Download HTTP Extraction**
  * `otaripper` intelligently parses remote OTA zip files and streams *only the exact byte-ranges* needed for your requested partitions over the network.
* **Android Native CLI Fixes**
  * Downgraded `reqwest` to `0.12.9` to eliminate a Java-based platform verifier dependency that was causing native panics inside Android terminal environments.
  * Implemented a dynamic OS-level fallback to strictly use the native OS resolver on Android, circumventing issues with custom ROMs that have broken `/etc/resolv.conf` symlinks.
* **Linux/Windows DNS Stability**
  * Transitioned Linux and Windows builds to use the pure-Rust `hickory-dns` resolver. This completely bypasses a known bug in `musl libc` that caused dropped cold connections, stabilizing remote HTTP extraction on static Linux builds.

---

## Release Engineering

* **"Gold Standard" Master Checksums**
  * Overhauled the GitHub Actions CI pipeline to compile a single, master `checksums.txt` file per release.
  * This file contains all SHA-256 hashes for the distributed archives (`.tar.gz` and `.zip`), fully streamlining automated ingestion by package managers like Winget and AUR.
* **Internal Binary Verification**
  * Raw binary checksums (`otaripper.sha256`) are now safely enclosed *inside* the release archives, ensuring users can natively verify their extracted executables without cross-platform filename hash collisions.
* **Otaripper Lite Builds**
  * Added automated distribution of `otaripper-lite` binaries. These builds explicitly exclude network dependencies (no HTTP remote capabilities) to achieve an ultra-minimal footprint for local-only extraction workflows.

---

## **otaripper v2.3.0** (2026-05-07)

### Direct Zip Memory Mapping (Zero-Copy Extraction)

This release introduces a massive performance upgrade for extraction speeds by completely bypassing the initial `payload.bin` extraction step for standard OTA zips.

---

## Performance & Efficiency

* **Direct Memory Mapping for STORED Zips**
  * Added a new `MappedOffset` variant to `PayloadSource`.
  * If the OTA `.zip` is uncompressed (`STORED` method, which is standard for almost all OTAs), the tool now memory maps the original `.zip` straight from the disk.
  * Uses an offset to jump directly to where the payload data starts.
  * **Instant Start**: Progress bars now show up almost immediately. No more waiting for the pre-unzip lag.
  * **Disk Health**: Saves gigabytes of redundant SSD writes every time you extract a partition.
  * **Pure Efficiency**: Bypasses the initial zip extraction overhead completely, resulting in major zero-copy speed boosts.
* **Graceful Fallback**
  * If the zip happens to be compressed, it safely falls back to the old RAM/Temp File method to ensure stability and prevent crashes.

---

## UX / CLI Improvements

* **Improved `arbscan` Error Messages**
  * Added a customized error message when global extraction flags (like `-l`, `--strict`, or `--sanity`) are accidentally used with the `arb`/`arbscan` subcommand.
  * Clearly explains that the subcommand only accepts the `-n` / `--no-json` flag, preventing user confusion.

---

## Credits

Special thanks to **ArKT-7** for this massive performance win and implementing the direct memory mapping logic.

---
## **otaripper v2.2.2** (2026-05-05)

### Arbscan Integration

This release merges the standalone `arbscan` utility directly into the `otaripper` codebase as a native subcommand.

---

## New Features

* **`otaripper arbscan` subcommand**
  * Built-in support for analyzing OEM Anti-Rollback (ARB) metadata from Qualcomm bootloader images (e.g., `xbl_config.img`).
  * **Direct Payload Analysis**: Can now accept a `firmware.zip` or `payload.bin` directly! It hooks into the extraction engine to automatically and silently dump `xbl_config.img` for analysis.
  * Added convenient shorthand aliases `arb` and `scan` (e.g., `otaripper arb update.zip`).
  * Added `-n` short flag for `--no-json`.
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

