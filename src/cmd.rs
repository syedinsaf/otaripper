use crate::chromeos_update_engine::install_operation::Type;
use crate::chromeos_update_engine::{DeltaArchiveManifest, InstallOperation, PartitionUpdate};
use crate::payload::Payload;
use anyhow::{Context, Result, bail, ensure};
use bzip2::read::BzDecoder;
use chrono::Local;
use clap::{Parser, ValueHint};
use console::Style;
use crossbeam_channel::unbounded;
use ctrlc;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressFinish, ProgressStyle};
use memmap2::{Mmap, MmapMut};
use prost::Message;
use rayon::{ThreadPool, ThreadPoolBuilder};
use ring::digest::{SHA256, digest};
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::cmp::Reverse;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{env, slice};
use sysinfo::{MemoryRefreshKind, RefreshKind};
use tempfile::NamedTempFile;
use zip::ZipArchive;

const OPTIMAL_CHUNK_SIZE: usize = 64 * 1024; // 64 KiB: balances cache efficiency and overhead in chunked processing
const SIMD_THRESHOLD: usize = 1024; // 1 KiB: minimum size to justify SIMD overhead
const PROGRESS_UPDATE_FREQUENCY_HIGH: u8 = 2; // 2 Hz refresh when ≤32 partitions (smooth without flicker)
const PROGRESS_UPDATE_FREQUENCY_LOW: u8 = 1; // 1 Hz refresh when >32 partitions (prevents terminal spam)
// Android OTA payload specification limits
const MIN_BLOCK_SIZE: usize = 512;
const MAX_BLOCK_SIZE: usize = 16 * 1024 * 1024; // 16 MiB

#[derive(Debug, Parser)]
#[clap(
    about,
    author,
    help_template = FRIENDLY_HELP,
    propagate_version = true,
    version = env!("CARGO_PKG_VERSION"),
)]
pub struct Cmd {
    /// OTA file, either a .zip file or a payload.bin.
    #[clap(short = 'p', long = "path", value_hint = ValueHint::FilePath, value_name = "PATH")]
    payload: Option<PathBuf>,

    /// List partitions instead of extracting them
    #[clap(
        conflicts_with = "threads",
        conflicts_with = "output_dir",
        conflicts_with = "partitions",
        conflicts_with = "no_verify",
        long,
        short
    )]
    list: bool,

    /// Number of threads to use during extraction
    #[clap(long, short, value_name = "NUMBER")]
    threads: Option<usize>,

    /// Set output directory
    #[clap(long, short, value_hint = ValueHint::DirPath, value_name = "PATH")]
    output_dir: Option<PathBuf>,

    /// Dump only selected partitions (comma-separated)
    #[clap(long, value_delimiter = ',', value_name = "PARTITIONS")]
    partitions: Vec<String>,

    /// Skip file verification (dangerous!)
    #[clap(long, conflicts_with = "strict")]
    no_verify: bool,

    /// Require cryptographic hashes and enforce verification; fails if any required hash is missing
    #[clap(
        long,
        help = "Require manifest hashes for partitions and operations; enforce verification and fail if any required hash is missing."
    )]
    strict: bool,

    /// Compute and print SHA-256 of each extracted partition image
    #[clap(
        long,
        help = "Compute and print the SHA-256 of each extracted partition image. If the manifest lacks a hash, this may add one linear pass over the image."
    )]
    print_hash: bool,

    /// Run lightweight sanity checks on output images (e.g., detect all-zero images)
    #[clap(
        long,
        help = "Run quick sanity checks on output images and fail on obviously invalid content (e.g., all zeros)."
    )]
    sanity: bool,

    /// Print per-partition and total timing/throughput statistics after extraction
    #[clap(
        long,
        help = "Print per-partition and total timing/throughput statistics after extraction."
    )]
    stats: bool,

    /// Don't automatically open the extracted folder after completion
    #[clap(
        long,
        help = "Don't automatically open the extracted folder after completion."
    )]
    no_open_folder: bool,

    /// Positional argument for the payload file
    #[clap(value_hint = ValueHint::FilePath)]
    #[clap(index = 1, value_name = "PATH")]
    positional_payload: Option<PathBuf>,
}

pub enum PayloadSource {
    Mapped(Mmap),
    Owned(Vec<u8>),
    Temp(Mmap, NamedTempFile),
}

impl Deref for PayloadSource {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        match self {
            PayloadSource::Mapped(mmap) => mmap,
            PayloadSource::Owned(vec) => vec,
            PayloadSource::Temp(mmap, _) => mmap,
        }
    }
}

/// Writes data across multiple memory regions efficiently with optional hashing.
pub struct ExtentsWriter<'a, 'b> {
    extents: &'a mut [&'b mut [u8]],
    idx: usize,
    off: usize,
}
impl<'a, 'b> ExtentsWriter<'a, 'b> {
    /// Create a new ExtentsWriter for writing to the given extents.
    pub fn new(extents: &'a mut [&'b mut [u8]]) -> Self {
        Self {
            extents,
            idx: 0,
            off: 0,
        }
    }

    #[inline]
    fn has_capacity(&self) -> bool {
        self.idx < self.extents.len()
    }

    #[inline]
    fn current_extent_capacity(&self) -> usize {
        if self.idx < self.extents.len() {
            self.extents[self.idx].len().saturating_sub(self.off)
        } else {
            0
        }
    }

    /// Write data using optimized copying strategies with SIMD acceleration
    #[inline(always)]
    fn write_to_current_extent(&mut self, data: &[u8]) -> usize {
        let available = self.current_extent_capacity();
        if available == 0 || data.is_empty() {
            return 0;
        }
        let to_copy = available.min(data.len());

        // Bounds are guaranteed: current_extent_capacity() returned non-zero,
        // which ensures self.idx is valid and to_copy fits within the extent
        let extent = &mut self.extents[self.idx];
        let dest_slice = &mut extent[self.off..self.off + to_copy];
        let src_slice = &data[..to_copy];

        // Hot path first: large copies (>= 1KB) use SIMD — this is the common case
        if to_copy >= SIMD_THRESHOLD {
            simd_copy_large(src_slice, dest_slice);
        } else {
            dest_slice.copy_from_slice(src_slice);
        }

        self.off += to_copy;
        if self.off >= extent.len() {
            self.idx += 1;
            self.off = 0;
        }
        to_copy
    }
}

impl<'a, 'b> io::Write for ExtentsWriter<'a, 'b> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut total_written = 0;

        // Write to available extents
        while !buf.is_empty() && self.has_capacity() {
            let written = self.write_to_current_extent(buf);
            if written == 0 {
                // This shouldn't happen if has_capacity() is true, but let's be safe
                break;
            }

            total_written += written;
            buf = &buf[written..];
        }

        Ok(total_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runtime CPU feature detection for SIMD acceleration.
/// Uses `OnceLock` for thread-safe lazy initialization.
/// Debug output enabled via `OTARIPPER_DEBUG_CPU=1`.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
enum CpuSimd {
    None,
    Sse2,
    Avx2,
    Avx512,
}

#[cfg(target_arch = "x86_64")]
impl CpuSimd {
    fn detect() -> Self {
        let avx512f = is_x86_feature_detected!("avx512f");
        let avx512bw = is_x86_feature_detected!("avx512bw");
        let avx2 = is_x86_feature_detected!("avx2");
        let sse2 = is_x86_feature_detected!("sse2");

        // Only log when explicitly requested via environment variable
        if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
            eprintln!("CPU Feature Detection:");
            eprintln!("  AVX512F: {}", avx512f);
            eprintln!("  AVX512BW: {}", avx512bw);
            eprintln!("  AVX2: {}", avx2);
            eprintln!("  SSE2: {}", sse2);
        }

        if avx512f && avx512bw {
            if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
                eprintln!("  Selected: AVX512");
            }
            CpuSimd::Avx512
        } else if avx2 {
            if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
                eprintln!("  Selected: AVX2");
            }
            CpuSimd::Avx2
        } else if sse2 {
            if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
                eprintln!("  Selected: SSE2");
            }
            CpuSimd::Sse2
        } else {
            if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
                eprintln!("  Selected: None (fallback to scalar)");
            }
            CpuSimd::None
        }
    }

    fn get() -> Self {
        use std::sync::OnceLock;
        static DETECTED: OnceLock<CpuSimd> = OnceLock::new();
        *DETECTED.get_or_init(CpuSimd::detect)
    }
}

// For non-x86_64 targets, we use a simple fallback enum
#[cfg(not(target_arch = "x86_64"))]
#[derive(Debug, Clone, Copy)]
enum CpuSimd {
    None,
}

#[cfg(not(target_arch = "x86_64"))]
impl CpuSimd {
    fn get() -> Self {
        if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
            eprintln!("CPU Feature Detection: ARM64/Other architecture - using scalar operations");
        }
        CpuSimd::None
    }
}

/// SIMD-optimized large data copying
#[inline]
fn simd_copy_large(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());

    // For very large transfers, process in cache-friendly chunks
    if src.len() > OPTIMAL_CHUNK_SIZE * 4 {
        let mut src_offset = 0;
        let mut dst_offset = 0;

        while src_offset < src.len() {
            let chunk_size = std::cmp::min(OPTIMAL_CHUNK_SIZE, src.len() - src_offset);
            let src_chunk = &src[src_offset..src_offset + chunk_size];
            let dst_chunk = &mut dst[dst_offset..dst_offset + chunk_size];

            simd_copy_chunk(src_chunk, dst_chunk);

            src_offset += chunk_size;
            dst_offset += chunk_size;
        }
    } else {
        simd_copy_chunk(src, dst);
    }
}

#[inline(always)]
fn simd_copy_chunk(src: &[u8], dst: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        match CpuSimd::get() {
            CpuSimd::Avx512 => unsafe {
                if src.len() >= 1_048_576 {
                    simd_copy_avx512_stream(src, dst);
                } else {
                    simd_copy_avx512(src, dst);
                }
            },
            CpuSimd::Avx2 => unsafe {
                if src.len() >= 1_048_576 {
                    simd_copy_avx2_stream(src, dst);
                } else {
                    simd_copy_avx2(src, dst);
                }
            },
            CpuSimd::Sse2 => unsafe { simd_copy_sse2(src, dst) },
            CpuSimd::None => dst.copy_from_slice(src),
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        dst.copy_from_slice(src);
    }
}
/// Public function: zero-check with SIMD auto-dispatch
#[inline(always)]
fn is_all_zero(data: &[u8]) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        match CpuSimd::get() {
            CpuSimd::Avx512 => unsafe { is_all_zero_avx512(data) },
            CpuSimd::Avx2 => unsafe { is_all_zero_avx2(data) },
            CpuSimd::Sse2 => unsafe { is_all_zero_sse2(data) },
            CpuSimd::None => data.iter().all(|&b| b == 0),
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        data.iter().all(|&b| b == 0)
    }
}

// === SIMD Copy Implementations ===
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
#[inline]
unsafe fn simd_copy_avx512(src: &[u8], dst: &mut [u8]) {
    let len = src.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(63);

    while i < simd_end {
        unsafe {
            let data = _mm512_loadu_si512(src_ptr.add(i) as *const __m512i);
            _mm512_storeu_si512(dst_ptr.add(i) as *mut __m512i, data);
        }
        i += 64;
    }

    if i < len {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn simd_copy_avx512_stream(src: &[u8], dst: &mut [u8]) {
    let len = src.len();

    if len < 1_048_576 {
        unsafe {
            return simd_copy_avx512(src, dst);
        }
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;

    // Work in 64-byte blocks
    let simd_end = len & !63;
    while i < simd_end {
        unsafe {
            let data = _mm512_loadu_si512(src_ptr.add(i) as *const __m512i);
            _mm512_stream_si512(dst_ptr.add(i) as *mut __m512i, data);
        }
        i += 64;
    }
    unsafe {
        _mm_sfence(); // CRITICAL: Flushes non-temporal store buffers to RAM.
        // This ensures data is globally visible before we signal
        // that this operation is complete.
    }
    // Tail
    if i < len {
        dst[i..].copy_from_slice(&src[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn simd_copy_avx2(src: &[u8], dst: &mut [u8]) {
    let len = src.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(31);

    while i < simd_end {
        unsafe {
            let data = _mm256_loadu_si256(src_ptr.add(i) as *const __m256i);
            _mm256_storeu_si256(dst_ptr.add(i) as *mut __m256i, data);
        }
        i += 32;
    }

    if i < len {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn simd_copy_avx2_stream(src: &[u8], dst: &mut [u8]) {
    let len = src.len();

    if len < 1_048_576 {
        unsafe {
            return simd_copy_avx2(src, dst);
        }
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;

    // Work in 32-byte blocks
    let simd_end = len & !31;
    while i < simd_end {
        unsafe {
            let data = _mm256_loadu_si256(src_ptr.add(i) as *const __m256i);
            _mm256_stream_si256(dst_ptr.add(i) as *mut __m256i, data);
        }
        i += 32;
    }

    unsafe {
        _mm_sfence(); // CRITICAL: Flushes non-temporal store buffers to RAM.
        // This ensures data is globally visible before we signal
        // that this operation is complete.
    }
    // Tail
    if i < len {
        dst[i..].copy_from_slice(&src[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
unsafe fn simd_copy_sse2(src: &[u8], dst: &mut [u8]) {
    let len = src.len();
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(15);

    while i < simd_end {
        unsafe {
            let data = _mm_loadu_si128(src_ptr.add(i) as *const __m128i);
            _mm_storeu_si128(dst_ptr.add(i) as *mut __m128i, data);
        }
        i += 16;
    }

    if i < len {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}

// === SIMD Zero-Check Implementations ===
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
#[inline]
unsafe fn is_all_zero_avx512(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(63);

    while i < simd_end {
        unsafe {
            let chunk = _mm512_loadu_si512(ptr.add(i) as *const __m512i);
            let zero = _mm512_setzero_si512();
            let cmp = _mm512_cmpeq_epi8_mask(chunk, zero);
            if cmp != u64::MAX {
                return false;
            }
        }
        i += 64;
    }

    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn is_all_zero_avx2(data: &[u8]) -> bool {
    let len: usize = data.len();
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(31);

    while i < simd_end {
        unsafe {
            let chunk = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
            let zero = _mm256_setzero_si256();
            let cmp = _mm256_cmpeq_epi8(chunk, zero);
            let mask = _mm256_movemask_epi8(cmp);
            if mask != -1 {
                return false;
            }
        }
        i += 32;
    }

    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
unsafe fn is_all_zero_sse2(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = len.saturating_sub(15);

    while i < simd_end {
        unsafe {
            let chunk = _mm_loadu_si128(ptr.add(i) as *const __m128i);
            let zero = _mm_setzero_si128();
            let cmp = _mm_cmpeq_epi8(chunk, zero);
            let mask = _mm_movemask_epi8(cmp);
            if mask != 0xFFFF {
                return false;
            }
            i += 16;
        }
    }
    data[i..].iter().all(|&b| b == 0)
}

// Main extraction loop: process partitions in descending size order
// for better progress bar visibility and cache behavior.
impl Cmd {
    pub fn run(&self) -> Result<()> {
        // Initialize SIMD detection early - this ensures SIMD capabilities are
        // detected and available for all operations throughout the extraction
        let _simd_level = CpuSimd::get();
        if let Some(t) = self.threads {
            match t {
                0 => { /* Use default - valid */ }
                1..=256 => { /* Valid range */ }
                _ => {
                    bail!(
                        "Thread count must be between 1 and 256, got {}. \
                        Use 0 or omit -t to use all available CPU cores (recommended).",
                        t
                    );
                }
            }
        }

        let payload_path = self.payload.as_ref().or(self.positional_payload.as_ref())
            .ok_or_else(|| anyhow::anyhow!(
                "No payload file specified. Please provide a payload file using -p/--path or as a positional argument.\n\nExamples:\n  otaripper payload.bin\n  otaripper -p ota.zip\n  otaripper --path update.zip"
            ))?
            .clone();

        // Proceed with the rest of the method using payload_path
        let payload = self.open_payload_file(&payload_path)?;
        // Because PayloadSource implements Deref, this call works seamlessly.
        let payload = &Payload::parse(&payload)?;

        let mut manifest =
            DeltaArchiveManifest::decode(payload.manifest).context("unable to parse manifest")?;

        // 1. Identify if the payload contains any incremental operations
        let has_incremental_ops = manifest.partitions.iter().any(|p| {
            p.operations.iter().any(|op| {
                matches!(
                    Type::try_from(op.r#type),
                    Ok(Type::SourceCopy
                        | Type::SourceBsdiff
                        | Type::BrotliBsdiff
                        | Type::Puffdiff
                        | Type::Zucchini)
                )
            })
        });
        let block_size = manifest.block_size.context(
            "The update file is missing critical metadata (block_size). It is likely corrupted.",
        )? as usize;
        ensure!(
            (MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size),
            "The update file has an invalid internal structure (block size {} is unsupported). It may be corrupted.",
            block_size,
        );

        // 2. LIST MODE: We allow this even for incremental files so the user can see the contents.
        if self.list {
            manifest
                .partitions
                .sort_unstable_by(|p1, p2| p1.partition_name.cmp(&p2.partition_name));

            println!("{:<20} {:<16} {:<10}", "Partition", "Size", "Type");
            println!("{:-<46}", "");

            for partition in &manifest.partitions {
                let size = partition
                    .new_partition_info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(|size| indicatif::HumanBytes(size).to_string());
                let size_str = size.as_deref().unwrap_or("???");

                // Determine if this specific partition is a patch or a full image
                let is_patch = partition.operations.iter().any(|op| {
                    matches!(
                        Type::try_from(op.r#type),
                        Ok(Type::SourceCopy
                            | Type::SourceBsdiff
                            | Type::BrotliBsdiff
                            | Type::Puffdiff
                            | Type::Zucchini)
                    )
                });

                let type_label = if is_patch {
                    Style::new().bold().red().apply_to("Incremental")
                } else {
                    Style::new().bold().green().apply_to("Full")
                };

                let name_style = Style::new().bold().green();
                println!(
                    "{:<20} {:<16} {:<10}",
                    name_style.apply_to(&partition.partition_name),
                    size_str,
                    type_label
                );
            }
            return Ok(());
        }

        // 3. EXTRACTION GUARD: Now we block extraction if it's incremental.
        if has_incremental_ops {
            let bold_cyan = Style::new().bold().cyan();
            let bold_yellow = Style::new().bold().yellow();

            bail!(
                "\n{header}\n\n\
                This file is an {incremental} update (patch). It only contains the {changes} \
                made between two versions, not the full system images.\n\n\
                {stop} {tool_name} only supports {full_ota} images.\n\n\
                {tip} Look for a larger zip (usually 2GB+) often labeled {factory} or {sideload} on OEM websites.\n",
                header = Style::new()
                    .bold()
                    .red()
                    .apply_to("❌ Extraction Not Possible"),
                incremental = bold_cyan.apply_to("incremental"),
                changes = bold_yellow.apply_to("binary changes"),
                stop = Style::new().dim().apply_to("Note:"),
                tool_name = env!("CARGO_PKG_NAME"),
                full_ota = bold_cyan.apply_to("Full OTA"),
                tip = Style::new().bold().green().apply_to("📌 Tip:"),
                factory = bold_yellow.apply_to("\"Full OTA\""),
                sideload = bold_yellow.apply_to("\"Recovery Flashable\"")
            );
        }

        // 4. Continue with extraction setup...
        for partition in &self.partitions {
            if !manifest
                .partitions
                .iter()
                .any(|p| &p.partition_name == partition)
            {
                bail!("partition \"{}\" not found in manifest", partition);
            }
        }
        // Sort partitions by size (descending).
        // Processing larger partitions first improves threadpool utilization and
        // ensures the most time-consuming progress bars start immediately.
        manifest.partitions.sort_unstable_by_key(|partition| {
            Reverse(
                partition
                    .new_partition_info
                    .as_ref()
                    .and_then(|info| info.size)
                    .unwrap_or(0),
            )
        });

        // Optional stats state
        let total_start = if self.stats {
            Some(Instant::now())
        } else {
            None
        };
        #[derive(Clone)]
        struct Stat {
            name: String,
            bytes: u64,
            ms: u128,
        }
        // Use channels to minimize contention: workers send Stat structs to a receiver
        let (stats_sender, stats_receiver) = if self.stats {
            let (s, r) = unbounded::<Stat>();
            (Some(s), Some(r))
        } else {
            (None, None)
        };

        // Optional hash records for clean printing after extraction
        #[derive(Clone)]
        struct HashRec {
            order: usize,
            name: String,
            hex: String,
        }
        // Channel for hash records
        let (hash_sender, hash_receiver) = if self.print_hash {
            let (s, r) = unbounded::<HashRec>();
            (Some(s), Some(r))
        } else {
            (None, None)
        };

        // Count selected partitions for progress redraw heuristic
        let selected_count: usize = manifest
            .partitions
            .iter()
            .filter(|u| self.partitions.is_empty() || self.partitions.contains(&u.partition_name))
            .count();

        // Strict mode sanity: ensure hashes exist when required
        if self.strict {
            for update in &manifest.partitions {
                if self.partitions.is_empty() || self.partitions.contains(&update.partition_name) {
                    // Partition-level hash must exist
                    ensure!(
                        update
                            .new_partition_info
                            .as_ref()
                            .and_then(|i| i.hash.as_ref())
                            .is_some(),
                        "strict mode: missing partition hash for '{}'",
                        update.partition_name
                    );
                    // Operation-level hashes must exist when data is present
                    for op in &update.operations {
                        if op.data_length.unwrap_or(0) > 0 {
                            ensure!(
                                op.data_sha256_hash.is_some(),
                                "strict mode: missing data_sha256_hash for an operation in '{}'",
                                update.partition_name
                            );
                        }
                    }
                }
            }
        }

        // Create/ensure output directory and detect if it was newly created
        let (partition_dir, created_new_dir) = self.create_partition_dir()?;

        let cleanup_state = Arc::new(Mutex::new((
            Vec::<PathBuf>::new(),
            partition_dir.to_path_buf(),
            created_new_dir,
        )));

        let cancellation_token = Arc::new(AtomicBool::new(false));

        let cleanup_state_ctrlc = Arc::clone(&cleanup_state);
        let cancellation_token_ctrlc = Arc::clone(&cancellation_token);

        ctrlc::set_handler(move || {
            eprintln!("\n\nReceived interrupt signal (Ctrl+C). Cleaning up and exiting...");

            // Signal all workers to abort
            cancellation_token_ctrlc.store(true, Ordering::Release);

            // Best-effort cleanup — avoid blocking in signal handler
            if let Ok(state) = cleanup_state_ctrlc.try_lock() {
                let (files, dir, dir_is_new) = &*state;

                if !files.is_empty() {
                    eprintln!("Removing {} partially extracted file(s)...", files.len());
                    let mut removed = 0;
                    for file in files.iter() {
                        if file.exists() {
                            if fs::remove_file(file).is_ok() {
                                removed += 1;
                            } else {
                                eprintln!("  ⚠️ Failed to remove: {}", file.display());
                            }
                        }
                    }
                    if removed > 0 {
                        eprintln!("Cleaned up {} partial file(s).", removed);
                    }
                }

                if *dir_is_new && dir.exists() {
                    if fs::remove_dir_all(dir).is_ok() {
                        eprintln!("Removed temporary extraction directory: {}", dir.display());
                    } else {
                        eprintln!(
                            "⚠️ Failed to remove extraction directory: {}",
                            dir.display()
                        );
                    }
                }
            } else {
                eprintln!(
                    "⚠️ Warning: Could not acquire cleanup lock (likely due to a thread panic)."
                );
                eprintln!("   Please manually check your output directory for partial files.");
            }

            eprintln!("✨ Goodbye!");
            std::process::exit(130);
        })
        .context("Failed to set up Ctrl+C handler")?;

        // Cleanup state: tracks files to delete and directory info for error cleanup
        let threadpool = self.get_threadpool()?;

        // Set up panic hook to trigger cleanup on any thread panic
        let cleanup_state_clone = Arc::clone(&cleanup_state);
        std::panic::set_hook(Box::new(move |_panic_info| {
            if let Ok(state) = cleanup_state_clone.lock() {
                let (files, dir, dir_is_new) = &*state;
                // Try to remove created files
                for f in files {
                    let _ = fs::remove_file(f);
                }
                // If we created the directory, try to remove it as well
                if *dir_is_new {
                    let _ = fs::remove_dir_all(dir);
                }
                eprintln!(
                    "Extraction aborted due to an error. Any partially extracted partition images have been deleted to prevent misuse."
                );
            }
        }));

        // Inform the user about effective concurrency when -t/--threads is provided
        if let Some(t) = self.threads
            && t > 0
        {
            eprintln!(
                "Using {} worker thread(s)",
                threadpool.current_num_threads()
            );
        }

        let bold_bright_red = Style::new().bold().red();
        let bold_yellow = Style::new().bold().yellow();
        let bold_bright_green = Style::new().bold().green();
        eprintln!(
            "\n{}: do {} close this window! Use {} to cancel safely.",
            bold_yellow.apply_to("Extraction in progress"),
            bold_bright_red.apply_to("NOT"),
            bold_bright_green.apply_to("Ctrl+C")
        );
        eprintln!(
            "Processing {} partitions using {} threads...",
            selected_count,
            threadpool.current_num_threads()
        );
        eprintln!();
        threadpool.scope(|scope| -> Result<()> {
            let multiprogress = {
                // Setting a fixed update frequency reduces flickering.
                let hz = if selected_count > 32 { PROGRESS_UPDATE_FREQUENCY_LOW } else { PROGRESS_UPDATE_FREQUENCY_HIGH };
                let draw_target = ProgressDrawTarget::stderr_with_hz(hz);
                MultiProgress::with_draw_target(draw_target)
            };
            // Maintain the manifest/extraction order for neatly printing hashes later
            for (hash_index_counter, update) in manifest
                .partitions
                .iter()
                .filter(|update| {
                    self.partitions.is_empty() || self.partitions.contains(&update.partition_name)
                })
                .enumerate()
            {
                self.validate_non_overlapping_extents(&update.operations)
                    .with_context(|| format!("Invalid extents in partition '{}'", update.partition_name))?;
                if cancellation_token.load(Ordering::Acquire) {
                    eprintln!("Extraction cancelled before processing '{}'", update.partition_name);
                    break;
                }
                let progress_bar = self.create_progress_bar(update)?;
                let progress_bar = multiprogress.add(progress_bar);
                let (partition_file, partition_len, out_path) =
                    self.open_partition_file(update, &partition_dir)?;
                // Track the file we just created for cleanup in case of errors
                if let Ok(mut state) = cleanup_state.lock() {
                    state.0.push(out_path);
                }

                let part_start = if self.stats { Some(Instant::now()) } else { None };
                let stats_sender = stats_sender.clone();

                // Assign an order index for hash printing
                let part_index = hash_index_counter;
                let hash_sender = hash_sender.clone();

                let remaining_ops = Arc::new(AtomicUsize::new(update.operations.len()));

                for op in update.operations.iter() {
                    let progress_bar = progress_bar.clone();
                    let partition_file = Arc::clone(&partition_file);
                    let remaining_ops = Arc::clone(&remaining_ops);


                    let part_name = update.partition_name.clone();
                    let stats_sender = stats_sender.clone();
                    let partition_len_for_stats = partition_len;
                    let hash_sender = hash_sender.clone();
                    let cancellation_token = Arc::clone(&cancellation_token);
                    scope.spawn(move |_| {
                        if cancellation_token.load(Ordering::Acquire) {
                            return;
                        }
                        let result = {
                            // We drop the ReadGuard immediately to avoid any reference aliasing.
                            let (base_ptr, partition_len) = {
                                let slice: &[u8] = &partition_file;
                                (slice.as_ptr() as *mut u8, slice.len())
                            };

                            self.run_op_raw(
                                op,
                                payload,
                                base_ptr,
                                partition_len,
                                block_size,
                            )
                        };
                        match result {
                            Ok(_) => {}
                            Err(e) => {
                                cancellation_token.store(true, Ordering::Release);
                                eprintln!("\nCritical error: Operation '{}' failed: {}", op.r#type, e);
                                eprintln!("Stopping extraction to prevent corrupted output...");
                                return;
                            }
                        }
                        if remaining_ops.fetch_sub(1, Ordering::AcqRel) == 1 {
                            // VERIFICATION PHASE: Exclusive access via write lock ensures all
                            // hardware store-buffers are synchronized and visible.



                            let final_slice: &[u8] = &partition_file;



                            // 1) Verification when enabled and hash provided
                            let mut computed_digest_opt: Option<[u8; 32]> = None;
                            if !self.no_verify {
                                if let Some(hash) = update
                                    .new_partition_info
                                    .as_ref()
                                    .and_then(|info| info.hash.as_ref())
                                {
                                    match self.verify_sha256_returning(final_slice, hash) {
                                        Ok(d) => computed_digest_opt = Some(d),
                                        Err(e) => {
                                            cancellation_token.store(true, Ordering::Release);
                                            eprintln!(
                                                "\nCritical error: Output verification failed for '{}': {}",
                                                part_name, e
                                            );
                                            eprintln!("Stopping extraction to prevent corrupted output...");
                                            return;
                                        }
                                    }
                                } else if self.strict {
                                    cancellation_token.store(true, Ordering::Release);
                                    eprintln!(
                                        "\nCritical error: Strict mode: missing partition hash for '{}'",
                                        part_name
                                    );
                                    eprintln!("Stopping extraction to prevent corrupted output...");
                                    return;
                                }
                            }


                            // Check cancellation before continuing
                            if cancellation_token.load(Ordering::Acquire) {
                                eprintln!("Post-processing for '{}' cancelled", part_name);
                                return;
                            }

                            // 2) Sanity checks (e.g., detect all-zero images)
                            if self.sanity
                                && is_all_zero(final_slice) {
                                    cancellation_token.store(true, Ordering::Release);
                                    eprintln!("\nCritical error: Sanity check failed for '{}': output image appears to be all zeros", part_name);
                                    eprintln!("Stopping extraction to prevent corrupted output...");
                                    return;
                                }

                            // Check cancellation before continuing
                            if cancellation_token.load(Ordering::Acquire) {
                                eprintln!("Post-processing for '{}' cancelled", part_name);
                                return;
                            }
                            // 3) Print SHA-256 if requested — reuse verified digest to avoid redundant work
                            if let Some(sender) = hash_sender.as_ref() {
                                let digest = if let Some(d) = computed_digest_opt {
                                    d
                                } else {
                                    let d = digest(&SHA256, final_slice);
                                        let mut arr = [0u8; 32];
                                        arr.copy_from_slice(d.as_ref());
                                        arr
                                    };
                                let hexstr = hex::encode(digest);
                                let _ = sender.send(HashRec { order: part_index, name: part_name.clone(), hex: hexstr });
                            }

                            // 4) Stats collection (optional)
                            if let (Some(start), Some(sender)) = (part_start, stats_sender.as_ref()) {
                                let elapsed = start.elapsed();
                                let _ = sender.send(Stat { name: part_name.clone(), bytes: partition_len_for_stats as u64, ms: elapsed.as_millis() });
                            }
                        }

                        progress_bar.inc(1);
                    });
                }
            }
            Ok(())
        })?;

        // Check if extraction was cancelled due to critical errors
        if cancellation_token.load(Ordering::Acquire) {
            // Clean up any partially extracted files
            if let Ok(state) = cleanup_state.lock() {
                let (files, dir, dir_is_new) = &*state;
                // Try to remove created files
                for f in files {
                    let _ = fs::remove_file(f);
                }
                // If we created the directory, try to remove it as well
                if *dir_is_new {
                    let _ = fs::remove_dir_all(dir);
                }
            }
            bail!(
                "Extraction was aborted due to critical errors. All partially extracted files have been removed."
            );
        }

        if let Ok(mut state) = cleanup_state.lock() {
            state.0.clear();
        }
        // Print partition hashes (cleanly) if requested
        if let Some(receiver) = hash_receiver.as_ref() {
            let mut v: Vec<HashRec> = Vec::new();
            while let Ok(r) = receiver.try_recv() {
                v.push(r);
            }
            if !v.is_empty() {
                v.sort_by_key(|r| r.order);
                println!("Partition hashes (SHA-256):");
                for r in v.iter() {
                    println!("{}: sha256={}", r.name, r.hex);
                }
            }
        }

        // Print stats summary if requested
        if let Some(receiver) = stats_receiver.as_ref() {
            let mut v: Vec<Stat> = Vec::new();
            while let Ok(s) = receiver.try_recv() {
                v.push(s);
            }
            if !v.is_empty() {
                let total_bytes: u64 = v.iter().map(|s| s.bytes).sum();
                let wall_ms = total_start.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                eprintln!("\nExtraction statistics:");
                for s in v.iter() {
                    let gbps = if s.ms > 0 {
                        (s.bytes as f64) / (s.ms as f64) / 1_000_000.0
                    } else {
                        0.0
                    };
                    eprintln!(
                        "  - {}: {} in {} ms ({:.2} GB/s)",
                        s.name,
                        indicatif::HumanBytes(s.bytes),
                        s.ms,
                        gbps
                    );
                }
                if wall_ms > 0 {
                    let total_gbps = (total_bytes as f64) / (wall_ms as f64) / 1_000_000.0;
                    eprintln!(
                        "  Total: {} in {} ms ({:.2} GB/s)",
                        indicatif::HumanBytes(total_bytes),
                        wall_ms,
                        total_gbps
                    );
                } else {
                    eprintln!("  Total: {}", indicatif::HumanBytes(total_bytes));
                }
            }
        }

        // If we got here, everything succeeded; clear cleanup state
        if let Ok(mut state) = cleanup_state.lock() {
            state.0.clear(); // Clear the file list so no cleanup happens
        }

        // Calculate and display extracted folder size
        self.display_extracted_folder_size(&partition_dir)?;

        // Automatically open the extracted folder (unless disabled)
        if !self.no_open_folder {
            self.open_extracted_folder(&partition_dir)?;
        }

        Ok(())
    }

    fn create_progress_bar(&self, update: &PartitionUpdate) -> Result<ProgressBar> {
        let finish = ProgressFinish::AndLeave;
        let style = ProgressStyle::with_template(
            "{prefix:>16!.green.bold} [{wide_bar:.white.dim}] {percent:>3.white}%",
        )
        .context("unable to build progress bar template")?
        .progress_chars("=> ");
        let bar = ProgressBar::new(update.operations.len() as u64)
            .with_finish(finish)
            .with_prefix(update.partition_name.to_string())
            .with_style(style);
        Ok(bar)
    }

    /// # Safety
    /// This function is the core of otaripper's high-performance extraction.
    /// It is sound because:
    /// 1.`base_ptr` is guaranteed to be valid for `partition_len` as long as the
    ///   `partition_file` Mmap is alive.
    /// 2. Scoped threads (`rayon::scope`) ensure that worker threads cannot outlive
    ///   the `Mmap` lifetime.
    /// 3. `validate_non_overlapping_extents` proves that no two threads can receive
    ///   the same memory range, preventing data races and mutable aliasing UB.
    fn run_op_raw(
        &self,
        op: &InstallOperation,
        payload: &Payload,
        base_ptr: *mut u8,
        partition_len: usize,
        block_size: usize,
    ) -> Result<()> {
        let raw_extents = self.extract_dst_extents_raw(op, base_ptr, partition_len, block_size)?;

        // Convert to temporary &mut [u8] — safe because:
        // - Extents are non-overlapping (validated globally)
        // - No other thread writes to these exact byte ranges
        // - These slices are NOT derived from a shared RwLock guard
        let mut dst_extents: Vec<&mut [u8]> = raw_extents
            .into_iter()
            .map(|(ptr, len)| unsafe { slice::from_raw_parts_mut(ptr, len) })
            .collect();

        // Now delegate to existing logic
        match Type::try_from(op.r#type)? {
            Type::Replace => {
                let data = self.extract_data(op, payload)?;
                self.run_op_replace_slice(data, &mut dst_extents, block_size)
            }
            Type::ReplaceBz => {
                let data = self.extract_data(op, payload)?;
                let mut decoder = BzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size)
            }
            Type::ReplaceXz => {
                let data = self.extract_data(op, payload)?;
                let mut decoder = xz2::read::XzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size)
            }
            Type::Zero | Type::Discard => Ok(()),
            _ => bail!("Unsupported operation type"),
        }
    }

    fn run_op_replace(
        &self,
        reader: &mut impl Read,
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
    ) -> Result<()> {
        let dst_len = dst_extents.iter().map(|e| e.len()).sum::<usize>();
        let bytes_read = io::copy(reader, &mut ExtentsWriter::new(dst_extents))
            .context("failed to write to buffer")? as usize;
        let bytes_read_aligned = bytes_read
            .saturating_add(block_size.saturating_sub(1))
            .saturating_div(block_size)
            .saturating_mul(block_size);
        ensure!(
            bytes_read_aligned == dst_len,
            "more dst blocks than data, even with padding"
        );
        Ok(())
    }

    fn run_op_replace_slice(
        &self,
        data: &[u8],
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
    ) -> Result<()> {
        let bytes_read = data.len();
        let dst_len: usize = dst_extents.iter().map(|e| e.len()).sum();
        let bytes_read_aligned = bytes_read
            .saturating_add(block_size.saturating_sub(1))
            .saturating_div(block_size)
            .saturating_mul(block_size);
        ensure!(
            bytes_read_aligned == dst_len,
            "more dst blocks than data, even with padding"
        );
        let written = ExtentsWriter::new(dst_extents)
            .write(data)
            .context("failed to write to buffer")?;
        ensure!(
            written == bytes_read,
            "failed to write all data to destination extents"
        );
        Ok(())
    }

    fn open_payload_file(&self, path: &Path) -> Result<PayloadSource> {
        use sysinfo::System;
        use tempfile::NamedTempFile;

        // 1. Open the file and peek magic bytes to identify format
        let mut file = File::open(path)
            .with_context(|| format!("unable to open file for reading: {path:?}"))?;

        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)
            .context("Failed to read file header")?;
        file.seek(std::io::SeekFrom::Start(0))?;

        // 2. CASE: ZIP archive (PK\x03\x04)
        if &magic == b"PK\x03\x04" {
            let mut archive = ZipArchive::new(&file)
                .context("File has ZIP magic but is not a valid ZIP archive")?;

            if let Ok(mut zipfile) = archive.by_name("payload.bin") {
                let payload_size = zipfile.size();

                // LIGHTWEIGHT RAM CHECK: Only refresh memory stats to minimize overhead
                let mut sys = System::new_with_specifics(
                    RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
                );
                sys.refresh_memory();
                let available_ram = sys.available_memory();

                // HEURISTIC: Use temp file if payload > 50% available RAM to avoid OOM or Swap lag
                if payload_size > available_ram / 2 {
                    eprintln!(
                        "⚠️ Large payload detected ({}). Available RAM: {}. Using localized temp file for safety.",
                        indicatif::HumanBytes(payload_size),
                        indicatif::HumanBytes(available_ram)
                    );

                    // LOCALIZED TEMP: Create in output dir to prevent cross-partition copy performance hits
                    let temp_file = if let Some(ref out_dir) = self.output_dir {
                        fs::create_dir_all(out_dir)?;
                        NamedTempFile::new_in(out_dir)
                    } else {
                        NamedTempFile::new()
                    }
                    .context("Failed to create temporary file for payload extraction")?;

                    // Stream directly from ZIP to Disk
                    io::copy(&mut zipfile, &mut temp_file.as_file())
                        .context("Failed to stream payload.bin from ZIP to disk")?;

                    // SYNC: Ensure data is physically committed before mapping for correctness
                    temp_file.as_file().sync_all()?;

                    let mmap = unsafe { Mmap::map(temp_file.as_file()) }
                        .context("Failed to mmap streamed payload")?;

                    return Ok(PayloadSource::Temp(mmap, temp_file));
                }

                // RAM PATH: Small enough to fit comfortably in memory
                let mut buffer = Vec::with_capacity(payload_size as usize);
                zipfile
                    .read_to_end(&mut buffer)
                    .context("Failed to read payload.bin from ZIP into RAM")?;
                return Ok(PayloadSource::Owned(buffer));
            }
        }

        // 3. CASE: Raw payload.bin (Zero-copy mapping)
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("failed to mmap raw payload file: {path:?}"))?;

        Ok(PayloadSource::Mapped(mmap))
    }

    fn open_partition_file(
        &self,
        update: &PartitionUpdate,
        partition_dir: impl AsRef<Path>,
    ) -> Result<(Arc<MmapMut>, usize, PathBuf)> {
        let partition_len = update
            .new_partition_info
            .as_ref()
            .and_then(|info| info.size)
            .context("unable to determine output file size")?;

        let filename = Path::new(&update.partition_name).with_extension("img");
        let path: PathBuf = partition_dir.as_ref().join(filename);

        let mmap = {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("unable to open file for writing: {path:?}"))?;
            file.set_len(partition_len)?;
            unsafe { MmapMut::map_mut(&file) }
                .with_context(|| format!("failed to mmap file: {path:?}"))?
        };
        let partition = Arc::new(mmap);
        Ok((partition, partition_len as usize, path))
    }

    fn extract_data<'a>(&self, op: &InstallOperation, payload: &'a Payload) -> Result<&'a [u8]> {
        let data_len = op.data_length.context("data_length not defined")? as usize;
        let offset = op.data_offset.context("data_offset not defined")? as usize;

        let end_offset = offset
            .checked_add(data_len)
            .context("data_offset + data_length overflows")?;
        ensure!(
            end_offset <= payload.data.len(),
            "data range {}..{} exceeds payload size {}",
            offset,
            end_offset,
            payload.data.len()
        );

        let data = &payload.data[offset..end_offset];

        if !self.no_verify
            && let Some(hash) = &op.data_sha256_hash
        {
            self.verify_sha256(data, hash)
                .context("input verification failed")?;
        }
        Ok(data)
    }

    /// Extracts destination extents as (pointer, length) pairs — safe for concurrent use.
    fn extract_dst_extents_raw(
        &self,
        op: &InstallOperation,
        base_ptr: *mut u8,
        partition_len: usize,
        block_size: usize,
    ) -> Result<Vec<(*mut u8, usize)>> {
        let mut out = Vec::with_capacity(op.dst_extents.len());
        for extent in &op.dst_extents {
            let start_block = extent.start_block.context("missing start_block")? as usize;
            let num_blocks = extent.num_blocks.context("missing num_blocks")? as usize;

            let start = start_block
                .checked_mul(block_size)
                .context("start_block * block_size overflows")?;
            let len = num_blocks
                .checked_mul(block_size)
                .context("num_blocks * block_size overflows")?;

            ensure!(len != 0, "extent length cannot be zero");

            ensure!(
                start + len <= partition_len,
                "extent {}..{} exceeds partition size {}",
                start,
                start + len,
                partition_len
            );

            let ptr = unsafe { base_ptr.add(start) };
            out.push((ptr, len));
        }
        Ok(out)
    }
    // Same as verify_sha256, but returns the computed digest on success so it can be reused.
    fn verify_sha256_returning(&self, data: &[u8], exp_hash: &[u8]) -> Result<[u8; 32]> {
        let got = digest(&SHA256, data);
        ensure!(
            got.as_ref() == exp_hash,
            "hash mismatch: expected {}, got {}",
            hex::encode(exp_hash),
            hex::encode(got.as_ref())
        );
        let mut out = [0u8; 32];
        out.copy_from_slice(got.as_ref());
        Ok(out)
    }

    fn verify_sha256(&self, data: &[u8], exp_hash: &[u8]) -> Result<()> {
        self.verify_sha256_returning(data, exp_hash)?;
        Ok(())
    }

    fn validate_non_overlapping_extents(&self, operations: &[InstallOperation]) -> Result<()> {
        let mut all_extents: Vec<(u64, u64)> = operations
            .iter()
            .flat_map(|op| op.dst_extents.iter())
            .map(|extent| {
                let start = extent.start_block.context("missing start_block")?;
                let num_blocks = extent.num_blocks.context("missing num_blocks")?;
                let end = start
                    .checked_add(num_blocks)
                    .context("extent end overflows u64")?;
                Ok((start, end))
            })
            .collect::<Result<Vec<_>>>()?;
        all_extents.sort_unstable_by_key(|(start, _)| *start);
        let mut last_end = 0;
        for (start, end) in all_extents {
            ensure!(
                start >= last_end,
                "Overlapping destination extents detected: block {} overlaps with prior extent ending at {}",
                start,
                last_end
            );
            last_end = end;
        }
        Ok(())
    }
    fn create_partition_dir(&self) -> Result<(PathBuf, bool)> {
        let dir = match &self.output_dir {
            Some(output_base) => {
                let now = Local::now();
                let timestamp_folder = format!("{}", now.format("extracted_%Y-%m-%d_%H-%M-%S"));
                output_base.join(timestamp_folder)
            }
            None => {
                let now = Local::now();
                let current_dir = env::current_dir().with_context(|| {
                    "Failed to determine current directory. Please specify --output-dir explicitly."
                })?;
                let filename = format!("{}", now.format("extracted_%Y-%m-%d_%H-%M-%S"));
                current_dir.join(filename)
            }
        };
        let existed = dir.exists();
        fs::create_dir_all(&dir)
            .with_context(|| format!("could not create output directory: {dir:?}"))?;
        Ok((dir, !existed))
    }

    fn get_threadpool(&self) -> Result<ThreadPool> {
        let mut builder = ThreadPoolBuilder::new();
        if let Some(t) = self.threads
            && t > 0
        {
            builder = builder.num_threads(t);
        }
        builder.build().context("unable to start threadpool")
    }

    /// Calculate and display the total size of the extracted folder
    fn display_extracted_folder_size(&self, partition_dir: impl AsRef<Path>) -> Result<()> {
        let dir_path = partition_dir.as_ref();

        // Calculate total size recursively
        let total_size = self.calculate_directory_size(dir_path)?;

        // Display the result
        println!("\nExtraction completed successfully!");
        println!("Output directory: {}", dir_path.display());
        println!(
            "Total extracted size: {}",
            indicatif::HumanBytes(total_size)
        );
        let bold_bright_blue = Style::new().bold().blue();
        println!(
            "Tool Source: {}",
            bold_bright_blue.apply_to("https://github.com/syedinsaf/otaripper")
        );
        Ok(())
    }

    /// Recursively calculate the size of a directory and its contents
    fn calculate_directory_size(&self, path: &Path) -> Result<u64> {
        if !path.exists() {
            return Ok(0);
        }

        let metadata = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for: {}", path.display()))?;

        if metadata.is_file() {
            return Ok(metadata.len());
        }

        if metadata.is_dir() {
            let mut total_size = 0u64;
            let entries = fs::read_dir(path)
                .with_context(|| format!("failed to read directory: {}", path.display()))?;

            for entry in entries {
                let entry = entry.with_context(|| {
                    format!("failed to read directory entry in: {}", path.display())
                })?;
                let entry_path = entry.path();
                total_size += self.calculate_directory_size(&entry_path)?;
            }
            return Ok(total_size);
        }
        Ok(0)
    }

    /// Automatically open the extracted folder in the default file manager
    fn open_extracted_folder(&self, partition_dir: impl AsRef<Path>) -> Result<()> {
        let dir_path = partition_dir.as_ref();

        // Only attempt to open if the directory exists
        if !dir_path.exists() {
            eprintln!("Warning: Output directory does not exist, cannot open folder");
            return Ok(());
        }

        // Cross-platform folder opening
        #[cfg(target_os = "windows")]
        {
            use std::process::Command;
            let _ = Command::new("explorer")
                .arg(dir_path)
                .spawn()
                .map_err(|e| eprintln!("Warning: Failed to open folder: {}", e));
        }

        #[cfg(target_os = "macos")]
        {
            use std::process::Command;
            let _ = Command::new("open")
                .arg(dir_path)
                .spawn()
                .map_err(|e| eprintln!("Warning: Failed to open folder: {}", e));
        }

        #[cfg(target_os = "linux")]
        {
            use std::process::Command;

            // On KDE, using xdg-open triggers a harmless but noisy Qt portal warning (related to kioclient registration).
            // Spawning Dolphin directly avoids this, while still opening the folder correctly.
            if Command::new("dolphin").arg(dir_path).spawn().is_ok() {
                return Ok(());
            }

            // Fallback to standard xdg-open for non-KDE desktops
            if Command::new("xdg-open").arg(dir_path).spawn().is_ok() {
                return Ok(());
            }

            eprintln!("Warning: Unable to open folder (dolphin and xdg-open failed)");
        }

        Ok(())
    }
}

const FRIENDLY_HELP: &str = color_print::cstr!(
    "\
{before-help}<bold><underline>{name} {version}</underline></bold>
{about}

<bold>QUICK START</bold>
  • Drag and drop an OTA .zip or payload.bin onto the executable.
  • Or run via command line: <cyan>otaripper update.zip</cyan>

<bold>COMMON TASKS</bold>
  • <bold>List</bold> partitions:      otaripper -l update.zip
  • <bold>Extract all</bold>:       otaripper update.zip
  • <bold>Extract specific</bold>:  otaripper update.zip --partitions boot,init_boot
  • <bold>Benchmarking</bold>:      otaripper update.zip --stats

<bold>SAFETY & INTEGRITY</bold>
  • SHA-256 verification is <green>enabled by default</green> for all operations.
  • Partial images are <red>automatically deleted</red> on error to prevent corruption.
  • Use <yellow>--strict</yellow> to enforce manifest hash requirements.

{usage-heading}
  {usage}

<bold>OPTIONS</bold>
{all-args}

<bold>PROJECT</bold>: <blue>https://github.com/syedinsaf/otaripper</blue>
{after-help}"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chromeos_update_engine::Extent;
    use rayon::ThreadPoolBuilder;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Helper to create a Cmd for testing
    fn test_cmd() -> Cmd {
        Cmd {
            payload: None,
            list: false,
            threads: None,
            output_dir: None,
            partitions: vec![],
            no_verify: false,
            strict: false,
            print_hash: false,
            sanity: false,
            stats: false,
            no_open_folder: false,
            positional_payload: None,
        }
    }

    // Test helper to create mock InstallOperation with extents
    fn mock_operation(extents: Vec<(u64, u64)>) -> InstallOperation {
        InstallOperation {
            dst_extents: extents
                .into_iter()
                .map(|(start, num)| Extent {
                    start_block: Some(start),
                    num_blocks: Some(num),
                })
                .collect(),
            r#type: 0,
            data_offset: Some(0),
            data_length: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn test_non_overlapping_valid_cases() {
        let cmd = test_cmd();

        // Case 1: Completely separate extents
        let ops = vec![
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(20, 10)]),
            mock_operation(vec![(40, 10)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Case 2: Adjacent extents (touching but not overlapping)
        let ops = vec![
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(10, 10)]),
            mock_operation(vec![(20, 10)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Case 3: Multiple extents per operation, no overlaps
        let ops = vec![
            mock_operation(vec![(0, 5), (10, 5)]),
            mock_operation(vec![(20, 5), (30, 5)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Case 4: Out of order input (should be sorted internally)
        let ops = vec![
            mock_operation(vec![(40, 10)]),
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(20, 10)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    #[test]
    fn test_overlapping_invalid_cases() {
        let cmd = test_cmd();

        // Case 1: Complete overlap
        let ops = vec![mock_operation(vec![(0, 10)]), mock_operation(vec![(5, 5)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Case 2: Partial overlap
        let ops = vec![mock_operation(vec![(0, 10)]), mock_operation(vec![(5, 10)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Case 3: Multiple operations with one overlap
        let ops = vec![
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(20, 10)]),
            mock_operation(vec![(25, 10)]), // Overlaps with previous
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Case 4: Same extent in two operations
        let ops = vec![
            mock_operation(vec![(10, 10)]),
            mock_operation(vec![(10, 10)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_edge_cases() {
        let cmd = test_cmd();

        // Case 1: Zero-length extent (should be valid, writes nothing)
        let ops = vec![mock_operation(vec![(0, 0)]), mock_operation(vec![(0, 10)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Case 2: Single operation with multiple non-overlapping extents
        let ops = vec![mock_operation(vec![(0, 10), (20, 10), (40, 10)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Case 3: Single operation with overlapping extents (invalid)
        let ops = vec![mock_operation(vec![(0, 10), (5, 10)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Case 4: Empty operations list
        let ops: Vec<InstallOperation> = vec![];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Case 5: Operation with no extents
        let ops = vec![mock_operation(vec![])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    #[test]
    fn test_overflow_protection() {
        let cmd = test_cmd();

        // Case 1: start_block + num_blocks would overflow u64
        let ops = vec![mock_operation(vec![(u64::MAX - 5, 10)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Case 2: Maximum valid extent
        let ops = vec![mock_operation(vec![(0, u64::MAX)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    // === CONCURRENT ACCESS TESTS (Version 2 specific) ===

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_concurrent_writes_to_disjoint_extents() {
        const BLOCK_SIZE: usize = 4096;
        const NUM_THREADS: usize = 8;
        const BLOCKS_PER_THREAD: usize = 100;

        // Create a mock partition file
        let total_size = NUM_THREADS * BLOCKS_PER_THREAD * BLOCK_SIZE;
        let mut data = vec![0u8; total_size];

        // Create operations with disjoint extents
        let operations: Vec<InstallOperation> = (0..NUM_THREADS)
            .map(|i| {
                let start_block = (i * BLOCKS_PER_THREAD) as u64;
                mock_operation(vec![(start_block, BLOCKS_PER_THREAD as u64)])
            })
            .collect();

        // Validate extents are non-overlapping
        let cmd = test_cmd();
        assert!(cmd.validate_non_overlapping_extents(&operations).is_ok());

        // Simulate concurrent writes (like Version 2)
        let write_count = Arc::new(AtomicUsize::new(0));
        let pool = ThreadPoolBuilder::new()
            .num_threads(NUM_THREADS)
            .build()
            .unwrap();

        // SAFETY: We're using the raw pointer pattern from your production code.
        // The pointer is valid for the lifetime of this function, and validate_non_overlapping_extents
        // ensures no two threads write to overlapping memory regions.
        let base_ptr = data.as_mut_ptr() as usize; // Convert to usize (which is Send)

        pool.scope(|scope| {
            for (thread_id, op) in operations.iter().enumerate() {
                let write_count = Arc::clone(&write_count);
                let base_ptr = base_ptr; // Capture the usize
                scope.spawn(move |_| {
                    // Extract extent range (like extract_dst_extents_raw)
                    let extent = &op.dst_extents[0];
                    let start_block = extent.start_block.unwrap() as usize;
                    let num_blocks = extent.num_blocks.unwrap() as usize;
                    let start_byte = start_block * BLOCK_SIZE;
                    let len_bytes = num_blocks * BLOCK_SIZE;

                    // Create mutable slice (safe because extents don't overlap)
                    // SAFETY: base_ptr is valid, and extent validation ensures no overlaps
                    let slice = unsafe {
                        std::slice::from_raw_parts_mut(
                            (base_ptr as *mut u8).add(start_byte),
                            len_bytes,
                        )
                    };

                    // Write unique pattern
                    slice.fill((thread_id + 1) as u8);
                    write_count.fetch_add(1, Ordering::SeqCst);
                });
            }
        });

        // Verify all writes completed
        assert_eq!(write_count.load(Ordering::SeqCst), NUM_THREADS);

        // Verify each extent has correct pattern (no overwrites)
        for (thread_id, op) in operations.iter().enumerate() {
            let extent = &op.dst_extents[0];
            let start_block = extent.start_block.unwrap() as usize;
            let num_blocks = extent.num_blocks.unwrap() as usize;
            let start_byte = start_block * BLOCK_SIZE;
            let len_bytes = num_blocks * BLOCK_SIZE;

            let expected_value = (thread_id + 1) as u8;
            assert!(
                data[start_byte..start_byte + len_bytes]
                    .iter()
                    .all(|&b| b == expected_value),
                "Thread {} extent was corrupted",
                thread_id
            );
        }
    }

    #[test]
    #[should_panic(expected = "overlapping")]
    fn test_concurrent_writes_detect_overlaps() {
        const BLOCK_SIZE: usize = 4096;

        let mut data = vec![0u8; 10 * BLOCK_SIZE];
        let _base_ptr = data.as_mut_ptr(); // Prefix with _ to silence warning

        // Create OVERLAPPING operations (intentionally invalid)
        let operations = vec![
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(5, 10)]), // Overlaps!
        ];

        let cmd = test_cmd();

        // This should panic/error before we even try concurrent writes
        cmd.validate_non_overlapping_extents(&operations)
            .expect("overlapping extents should be detected");
    }

    // === STRESS TEST ===

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_stress_many_small_extents() {
        const NUM_OPERATIONS: usize = 1000;
        const BLOCKS_PER_EXTENT: u64 = 10;

        let cmd = test_cmd();

        // Create many non-overlapping operations
        let operations: Vec<InstallOperation> = (0..NUM_OPERATIONS)
            .map(|i| {
                let start = (i as u64) * BLOCKS_PER_EXTENT;
                mock_operation(vec![(start, BLOCKS_PER_EXTENT)])
            })
            .collect();

        assert!(cmd.validate_non_overlapping_extents(&operations).is_ok());

        // Verify concurrent access would be safe
        let total_blocks = NUM_OPERATIONS as u64 * BLOCKS_PER_EXTENT;
        let total_bytes = total_blocks as usize * 4096;
        let mut data = vec![0u8; total_bytes];

        // SAFETY: Same pattern as production code - pointer is valid and extents don't overlap
        let base_ptr = data.as_mut_ptr() as usize; // Convert to usize (which is Send)

        let pool = ThreadPoolBuilder::new().build().unwrap();
        pool.scope(|scope| {
            for (idx, op) in operations.iter().enumerate() {
                let base_ptr = base_ptr; // Capture the usize
                scope.spawn(move |_| {
                    let extent = &op.dst_extents[0];
                    let start_byte = extent.start_block.unwrap() as usize * 4096;
                    let len_bytes = extent.num_blocks.unwrap() as usize * 4096;

                    // SAFETY: base_ptr is valid, extent validation ensures no overlaps
                    let slice = unsafe {
                        std::slice::from_raw_parts_mut(
                            (base_ptr as *mut u8).add(start_byte),
                            len_bytes,
                        )
                    };

                    slice.fill((idx % 256) as u8);
                });
            }
        });

        // Verify no corruption
        for (idx, op) in operations.iter().enumerate() {
            let extent = &op.dst_extents[0];
            let start_byte = extent.start_block.unwrap() as usize * 4096;
            let len_bytes = extent.num_blocks.unwrap() as usize * 4096;
            let expected = (idx % 256) as u8;

            assert!(
                data[start_byte..start_byte + len_bytes]
                    .iter()
                    .all(|&b| b == expected),
                "Extent {} corrupted",
                idx
            );
        }
    }

    // === PROPERTY-BASED TEST (using proptest if available) ===

    #[cfg(feature = "proptest")]
    use proptest::prelude::*;

    #[cfg(feature = "proptest")]
    proptest! {
        #[test]
        fn prop_non_overlapping_extents_are_valid(
            extents in prop::collection::vec((0u64..1000, 1u64..100), 1..50)
        ) {
            let cmd = test_cmd();

            // Sort and deduplicate to ensure non-overlapping
            let mut sorted_extents = extents;
            sorted_extents.sort_by_key(|(start, _)| *start);
            sorted_extents.dedup();

            // Create non-overlapping operations
            let mut last_end = 0u64;
            let operations: Vec<InstallOperation> = sorted_extents
                .into_iter()
                .filter_map(|(start, len)| {
                    if start >= last_end {
                        last_end = start + len;
                        Some(mock_operation(vec![(start, len)]))
                    } else {
                        None
                    }
                })
                .collect();

            if !operations.is_empty() {
                prop_assert!(cmd.validate_non_overlapping_extents(&operations).is_ok());
            }
        }

        #[test]
        fn prop_overlapping_extents_are_invalid(
            base_start in 0u64..1000,
            base_len in 10u64..100,
            overlap_offset in 1u64..9,
        ) {
            let cmd = test_cmd();

            // Create intentionally overlapping extents
            let operations = vec![
                mock_operation(vec![(base_start, base_len)]),
                mock_operation(vec![(base_start + overlap_offset, base_len)]),
            ];

            prop_assert!(cmd.validate_non_overlapping_extents(&operations).is_err());
        }
    }
    // Add these tests to your existing #[cfg(test)] mod tests block

    #[test]
    fn test_single_byte_extents() {
        let cmd = test_cmd();

        // Single-byte extents should work
        let ops = vec![
            mock_operation(vec![(0, 1)]),
            mock_operation(vec![(1, 1)]),
            mock_operation(vec![(2, 1)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Single-byte overlap
        let ops = vec![
            mock_operation(vec![(0, 2)]),
            mock_operation(vec![(1, 1)]), // Overlaps at byte 1
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_large_extent_values() {
        let cmd = test_cmd();

        // Very large but valid extent
        let ops = vec![
            mock_operation(vec![(0, 1_000_000_000)]),
            mock_operation(vec![(1_000_000_000, 1_000_000_000)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Large extent that would overflow when computing end
        let ops = vec![mock_operation(vec![(u64::MAX / 2, u64::MAX / 2 + 2)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_interleaved_extents() {
        let cmd = test_cmd();

        // Multiple operations with interleaved non-overlapping extents
        let ops = vec![
            mock_operation(vec![(0, 5), (20, 5), (40, 5)]),
            mock_operation(vec![(10, 5), (30, 5), (50, 5)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Interleaved with one overlap
        let ops = vec![
            mock_operation(vec![(0, 5), (20, 5), (40, 5)]),
            mock_operation(vec![(10, 5), (30, 5), (42, 5)]), // Overlaps at 42
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_many_operations_single_extent_each() {
        let cmd = test_cmd();

        // 100 operations, each with a single non-overlapping extent
        let ops: Vec<InstallOperation> = (0..100)
            .map(|i| mock_operation(vec![(i * 10, 5)]))
            .collect();
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Same but with one overlap in the middle
        let mut ops: Vec<InstallOperation> = (0..100)
            .map(|i| mock_operation(vec![(i * 10, 5)]))
            .collect();
        ops[50] = mock_operation(vec![(492, 10)]); // Overlaps with extent 49
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_gaps_between_extents() {
        let cmd = test_cmd();

        // Large gaps between extents (should be valid)
        let ops = vec![
            mock_operation(vec![(0, 1)]),
            mock_operation(vec![(1000, 1)]),
            mock_operation(vec![(1_000_000, 1)]),
            mock_operation(vec![(1_000_000_000, 1)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    #[test]
    fn test_reverse_order_extents_in_single_operation() {
        let cmd = test_cmd();

        // Extents within a single operation in reverse order (should still validate correctly)
        let ops = vec![mock_operation(vec![(40, 5), (20, 5), (0, 5)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Same but with overlap
        let ops = vec![mock_operation(vec![(40, 10), (20, 25)])]; // 20..45 overlaps with 40..50
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_maximum_block_numbers() {
        let cmd = test_cmd();

        // Extent at maximum possible start position with small size
        let ops = vec![mock_operation(vec![(u64::MAX - 100, 50)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Two extents near maximum
        let ops = vec![
            mock_operation(vec![(u64::MAX - 200, 50)]),
            mock_operation(vec![(u64::MAX - 100, 50)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    #[test]
    fn test_many_zero_length_extents() {
        let cmd = test_cmd();

        // Multiple zero-length extents at the same position (should be valid)
        let ops = vec![
            mock_operation(vec![(0, 0)]),
            mock_operation(vec![(0, 0)]),
            mock_operation(vec![(0, 0)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Mix of zero-length and normal extents
        let ops = vec![
            mock_operation(vec![(0, 0), (10, 5), (20, 0)]),
            mock_operation(vec![(0, 0), (15, 5), (25, 0)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    #[test]
    fn test_off_by_one_boundaries() {
        let cmd = test_cmd();

        // Extent ending exactly where next begins (adjacent, valid)
        let ops = vec![
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(10, 10)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Off by one - starts one block before previous ends (overlap)
        let ops = vec![mock_operation(vec![(0, 10)]), mock_operation(vec![(9, 10)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Off by one - starts one block after previous ends (gap, valid)
        let ops = vec![
            mock_operation(vec![(0, 10)]),
            mock_operation(vec![(11, 10)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
    }

    // === CONCURRENT EDGE CASE TESTS ===

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_concurrent_single_byte_writes() {
        const BLOCK_SIZE: usize = 4096;
        const NUM_THREADS: usize = 16;

        let total_size = NUM_THREADS * BLOCK_SIZE;
        let mut data = vec![0u8; total_size];

        // Each thread writes to a single block
        let operations: Vec<InstallOperation> = (0..NUM_THREADS)
            .map(|i| mock_operation(vec![(i as u64, 1)]))
            .collect();

        let cmd = test_cmd();
        assert!(cmd.validate_non_overlapping_extents(&operations).is_ok());

        let base_ptr = data.as_mut_ptr() as usize;
        let pool = ThreadPoolBuilder::new()
            .num_threads(NUM_THREADS)
            .build()
            .unwrap();

        pool.scope(|scope| {
            for (thread_id, op) in operations.iter().enumerate() {
                let base_ptr = base_ptr;
                scope.spawn(move |_| {
                    let extent = &op.dst_extents[0];
                    let start_byte = extent.start_block.unwrap() as usize * BLOCK_SIZE;
                    let len_bytes = extent.num_blocks.unwrap() as usize * BLOCK_SIZE;

                    let slice = unsafe {
                        std::slice::from_raw_parts_mut(
                            (base_ptr as *mut u8).add(start_byte),
                            len_bytes,
                        )
                    };

                    slice.fill((thread_id + 1) as u8);
                });
            }
        });

        // Verify each block
        for (thread_id, op) in operations.iter().enumerate() {
            let extent = &op.dst_extents[0];
            let start_byte = extent.start_block.unwrap() as usize * BLOCK_SIZE;
            let len_bytes = extent.num_blocks.unwrap() as usize * BLOCK_SIZE;
            let expected = (thread_id + 1) as u8;

            assert!(
                data[start_byte..start_byte + len_bytes]
                    .iter()
                    .all(|&b| b == expected),
                "Thread {} block was corrupted",
                thread_id
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_concurrent_interleaved_writes() {
        const BLOCK_SIZE: usize = 4096;

        let total_size = 10 * BLOCK_SIZE;
        let mut data = vec![0u8; total_size];

        // Create interleaved pattern: thread 0 writes to blocks 0,2,4,6,8
        // thread 1 writes to blocks 1,3,5,7,9
        let operations: Vec<InstallOperation> = vec![
            mock_operation(vec![(0, 1), (2, 1), (4, 1), (6, 1), (8, 1)]),
            mock_operation(vec![(1, 1), (3, 1), (5, 1), (7, 1), (9, 1)]),
        ];

        let cmd = test_cmd();
        assert!(cmd.validate_non_overlapping_extents(&operations).is_ok());

        let base_ptr = data.as_mut_ptr() as usize;
        let pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();

        pool.scope(|scope| {
            for (thread_id, op) in operations.iter().enumerate() {
                let base_ptr = base_ptr;
                scope.spawn(move |_| {
                    for extent in &op.dst_extents {
                        let start_byte = extent.start_block.unwrap() as usize * BLOCK_SIZE;
                        let len_bytes = extent.num_blocks.unwrap() as usize * BLOCK_SIZE;

                        let slice = unsafe {
                            std::slice::from_raw_parts_mut(
                                (base_ptr as *mut u8).add(start_byte),
                                len_bytes,
                            )
                        };

                        slice.fill((thread_id + 1) as u8);
                    }
                });
            }
        });

        // Verify interleaved pattern
        for block_idx in 0..10 {
            let start = block_idx * BLOCK_SIZE;
            let end = start + BLOCK_SIZE;
            let expected = if block_idx % 2 == 0 { 1 } else { 2 };

            assert!(
                data[start..end].iter().all(|&b| b == expected),
                "Block {} has wrong pattern",
                block_idx
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_concurrent_with_gaps() {
        const BLOCK_SIZE: usize = 4096;

        // Create data with gaps (blocks 1, 3, 5 are never written to)
        let total_size = 6 * BLOCK_SIZE;
        let mut data = vec![0xFFu8; total_size]; // Fill with 0xFF to detect unwritten areas

        let operations: Vec<InstallOperation> = vec![
            mock_operation(vec![(0, 1)]),
            mock_operation(vec![(2, 1)]),
            mock_operation(vec![(4, 1)]),
        ];

        let cmd = test_cmd();
        assert!(cmd.validate_non_overlapping_extents(&operations).is_ok());

        let base_ptr = data.as_mut_ptr() as usize;
        let pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();

        pool.scope(|scope| {
            for (thread_id, op) in operations.iter().enumerate() {
                let base_ptr = base_ptr;
                scope.spawn(move |_| {
                    let extent = &op.dst_extents[0];
                    let start_byte = extent.start_block.unwrap() as usize * BLOCK_SIZE;
                    let len_bytes = extent.num_blocks.unwrap() as usize * BLOCK_SIZE;

                    let slice = unsafe {
                        std::slice::from_raw_parts_mut(
                            (base_ptr as *mut u8).add(start_byte),
                            len_bytes,
                        )
                    };

                    slice.fill((thread_id + 1) as u8);
                });
            }
        });

        // Verify written blocks
        assert!(data[0..BLOCK_SIZE].iter().all(|&b| b == 1));
        assert!(data[2 * BLOCK_SIZE..3 * BLOCK_SIZE].iter().all(|&b| b == 2));
        assert!(data[4 * BLOCK_SIZE..5 * BLOCK_SIZE].iter().all(|&b| b == 3));

        // Verify gaps remain untouched (still 0xFF)
        assert!(data[BLOCK_SIZE..2 * BLOCK_SIZE].iter().all(|&b| b == 0xFF));
        assert!(
            data[3 * BLOCK_SIZE..4 * BLOCK_SIZE]
                .iter()
                .all(|&b| b == 0xFF)
        );
        assert!(
            data[5 * BLOCK_SIZE..6 * BLOCK_SIZE]
                .iter()
                .all(|&b| b == 0xFF)
        );
    }

    #[test]
    fn test_validation_with_duplicate_extents() {
        let cmd = test_cmd();

        // Exact duplicate extents in different operations (should fail)
        let ops = vec![mock_operation(vec![(10, 5)]), mock_operation(vec![(10, 5)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());

        // Duplicate extents in same operation (should also fail)
        let ops = vec![mock_operation(vec![(10, 5), (10, 5)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_validation_performance_many_extents() {
        let cmd = test_cmd();

        // Test with 10,000 non-overlapping extents (validation should be fast)
        let ops: Vec<InstallOperation> = (0..10000)
            .map(|i| mock_operation(vec![(i * 10, 5)]))
            .collect();

        let start = std::time::Instant::now();
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());
        let duration = start.elapsed();

        // Validation should complete in reasonable time (< 100ms)
        assert!(
            duration.as_millis() < 100,
            "Validation took too long: {:?}",
            duration
        );
    }

    #[test]
    fn test_extent_at_zero_with_max_size() {
        let cmd = test_cmd();

        // Extent starting at 0 with maximum possible size
        let ops = vec![mock_operation(vec![(0, u64::MAX)])];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Two extents where first is max size (should fail since second can't fit)
        let ops = vec![
            mock_operation(vec![(0, u64::MAX)]),
            mock_operation(vec![(u64::MAX, 1)]), // This would overflow when computing end
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }
    #[test]
    fn test_real_world_partition_pattern() {
        let cmd = test_cmd();

        // Simulate realistic Android OTA pattern:
        // - Boot partition: blocks 0-20000
        // - System partition: blocks 20000-500000
        // - Vendor partition: blocks 500000-600000
        let ops = vec![
            mock_operation(vec![(0, 20000)]),
            mock_operation(vec![(20000, 480000)]),
            mock_operation(vec![(500000, 100000)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Simulate corruption where system overlaps vendor by 1 block
        let ops = vec![
            mock_operation(vec![(0, 20000)]),
            mock_operation(vec![(20000, 480001)]), // Overlaps!
            mock_operation(vec![(500000, 100000)]),
        ];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }

    #[test]
    fn test_sparse_fragmented_partition() {
        let cmd = test_cmd();

        // Simulate highly fragmented partition (common in incremental updates)
        // Single operation with 50 small scattered extents
        let extents: Vec<(u64, u64)> = (0..50)
            .map(|i| (i * 1000, 10)) // 10 blocks every 1000 blocks
            .collect();
        let ops = vec![mock_operation(extents)];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_ok());

        // Same but with two fragments overlapping
        let mut extents: Vec<(u64, u64)> = (0..50).map(|i| (i * 1000, 10)).collect();
        extents[25] = (24005, 10); // Overlaps with previous fragment
        let ops = vec![mock_operation(extents)];
        assert!(cmd.validate_non_overlapping_extents(&ops).is_err());
    }
}
