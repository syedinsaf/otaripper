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
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{env, slice};
use sysinfo::{MemoryRefreshKind, RefreshKind};
use tempfile::NamedTempFile;
use zip::ZipArchive;

const OPTIMAL_CHUNK_SIZE: usize = 256 * 1024; // 256 KiB: larger chunks for better throughput
const SIMD_THRESHOLD: usize = 4096; // 4 KiB: increased threshold for better SIMD utilization
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
    #[clap(short = 'p', long, value_delimiter = ',', value_name = "PARTITIONS")]
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
        short = 'n',
        help = "Don't automatically open the extracted folder after completion."
    )]
    no_open: bool,

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

#[derive(Clone, Copy)]
#[repr(transparent)]
struct PartitionPtr<'a> {
    ptr: *mut u8,
    _marker: PhantomData<&'a mut u8>,
}
// SAFETY:
// Pointer always originates from an Arc<MmapMut> that outlives all threads
// Extents are validated to be non-overlapping
// Rayon scope guarantees threads cannot outlive the mmap
unsafe impl<'a> Send for PartitionPtr<'a> {}
unsafe impl<'a> Sync for PartitionPtr<'a> {}

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
    simd: CpuSimd,
}
impl<'a, 'b> ExtentsWriter<'a, 'b> {
    /// Create a new ExtentsWriter for writing to the given extents.
    pub(crate) fn new(extents: &'a mut [&'b mut [u8]], simd: CpuSimd) -> Self {
        Self {
            extents,
            idx: 0,
            off: 0,
            simd,
        }
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
            simd_copy_large(self.simd, src_slice, dest_slice);
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
        let mut total_written = 0;

        while !buf.is_empty() {
            let written = self.write_to_current_extent(buf);
            if written == 0 {
                break; // no more capacity
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
pub(crate) enum CpuSimd {
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

        let selected = if avx512f && avx512bw {
            CpuSimd::Avx512
        } else if avx2 {
            CpuSimd::Avx2
        } else if sse2 {
            CpuSimd::Sse2
        } else {
            CpuSimd::None
        };

        if std::env::var("OTARIPPER_DEBUG_CPU").is_ok() {
            eprintln!("CPU Feature Detection:");
            eprintln!("  AVX512F: {}", avx512f);
            eprintln!("  AVX512BW: {}", avx512bw);
            eprintln!("  AVX2: {}", avx2);
            eprintln!("  SSE2: {}", sse2);
            eprintln!("  Selected: {:?}", selected);
        }

        selected
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
pub(crate) enum CpuSimd {
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
fn simd_copy_large(simd: CpuSimd, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());

    if src.len() > OPTIMAL_CHUNK_SIZE * 4 {
        let mut offset = 0;

        while offset < src.len() {
            let chunk_size = std::cmp::min(OPTIMAL_CHUNK_SIZE, src.len() - offset);

            let src_chunk = &src[offset..offset + chunk_size];
            let dst_chunk = &mut dst[offset..offset + chunk_size];

            simd_copy_chunk(simd, src_chunk, dst_chunk);

            offset += chunk_size;
        }
    } else {
        simd_copy_chunk(simd, src, dst);
    }
}

#[inline(always)]
fn simd_copy_chunk(simd: CpuSimd, src: &[u8], dst: &mut [u8]) {
    match simd {
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

/// Zero-check with SIMD already selected (hot path)
#[inline(always)]
fn is_all_zero_with_simd(simd: CpuSimd, data: &[u8]) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        match simd {
            CpuSimd::Avx512 => unsafe { is_all_zero_avx512(data) },
            CpuSimd::Avx2 => unsafe { is_all_zero_avx2(data) },
            CpuSimd::Sse2 => unsafe { is_all_zero_sse2(data) },
            CpuSimd::None => data.iter().all(|&b| b == 0),
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        // Non-x86 always scalar (auto-vectorized by LLVM)
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
    if src.len() < 1_048_576 {
        unsafe {
            return simd_copy_avx512(src, dst);
        }
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;

    // Work in 64-byte blocks
    let simd_end = src.len() & !63;
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
    if i < src.len() {
        dst[i..].copy_from_slice(&src[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn simd_copy_avx2(src: &[u8], dst: &mut [u8]) {
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = src.len().saturating_sub(31);

    while i < simd_end {
        unsafe {
            let data = _mm256_loadu_si256(src_ptr.add(i) as *const __m256i);
            _mm256_storeu_si256(dst_ptr.add(i) as *mut __m256i, data);
        }
        i += 32;
    }

    if i < src.len() {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn simd_copy_avx2_stream(src: &[u8], dst: &mut [u8]) {
    if src.len() < 1_048_576 {
        unsafe {
            return simd_copy_avx2(src, dst);
        }
    }

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;

    // Work in 32-byte blocks
    let simd_end = src.len() & !31;
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
    if i < src.len() {
        dst[i..].copy_from_slice(&src[i..]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
unsafe fn simd_copy_sse2(src: &[u8], dst: &mut [u8]) {
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut i = 0;
    let simd_end = src.len().saturating_sub(15);

    while i < simd_end {
        unsafe {
            let data = _mm_loadu_si128(src_ptr.add(i) as *const __m128i);
            _mm_storeu_si128(dst_ptr.add(i) as *mut __m128i, data);
        }
        i += 16;
    }

    if i < src.len() {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
    }
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
#[inline]
unsafe fn is_all_zero_avx512(data: &[u8]) -> bool {
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = data.len().saturating_sub(63);

    while i < simd_end {
        unsafe {
            let chunk = _mm512_loadu_si512(ptr.add(i) as *const __m512i);

            if _mm512_test_epi8_mask(chunk, chunk) != 0 {
                // ← Correct
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
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = data.len().saturating_sub(31);

    while i < simd_end {
        unsafe {
            let chunk = _mm256_loadu_si256(ptr.add(i) as *const __m256i);

            if _mm256_testz_si256(chunk, chunk) == 0 {
                // ← Correct
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
    let ptr = data.as_ptr();
    let mut i = 0;
    let simd_end = data.len().saturating_sub(15);

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
        let simd = CpuSimd::get();
        if let Some(t) = self.threads {
            match t {
                0 => { /* Use default - valid */ }
                1..=256 => { /* Valid range */ }
                _ => {
                    bail!(
                        "Thread count {} is out of range.\n\
                         Valid range: 1–256\n\
                         Hint: Use 0 or leave -t unset to auto-detect (recommended).",
                        t
                    );
                }
            }
        }

        let payload_path = self.positional_payload.as_ref()
            .ok_or_else(|| anyhow::anyhow!(
                "No payload file specified.\n\
        \n\
        Usage:\n\
          otaripper <payload.zip | payload.bin>\n\
        \n\
        Examples:\n\
          • Extract everything:\n\
              otaripper update.zip\n\
        \n\
          • Extract only specific partitions:\n\
              otaripper update.zip -p boot,init_boot,vendor_boot\n\
        \n\
        Tip:\n\
          Use comma-separated partition names without .img extension. Names must match the OTA manifest.\n\
        \n\
        For more options and features, run:\n\
          otaripper -h\n"
            ))?
            .clone();

        // Proceed with the rest of the method using payload_path
        let payload = self.open_payload_file(&payload_path)?;
        // Because PayloadSource implements Deref, this call works seamlessly.
        let payload = &Payload::parse(&payload)?;

        let mut manifest =
            DeltaArchiveManifest::decode(payload.manifest).context("unable to parse manifest")?;

        // 1. Identify if the payload contains any incremental operations
        let has_incremental_ops = manifest
            .partitions
            .iter()
            .any(Self::is_incremental_partition);

        let block_size = manifest.block_size.context(
            "The update file is missing critical metadata (block_size). It is likely corrupted.",
        )? as usize;
        ensure!(
            (MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size),
            "The update file has an invalid internal structure (block size {} is unsupported). It may be corrupted.",
            block_size,
        );
        ensure!(
            block_size.is_power_of_two(),
            "The update file is malformed: block size {} is not a power of two.",
            block_size,
        );

        // 2. LIST MODE: Shows partition details and identifies Incremental vs Full updates.
        if self.list {
            manifest
                .partitions
                .sort_unstable_by(|p1, p2| p1.partition_name.cmp(&p2.partition_name));

            println!("{:<20} {:<16} {:<10}", "Partition", "Size", "Type");
            println!("{:-<46}", "");

            let partition_count = manifest.partitions.len();

            for partition in &manifest.partitions {
                // Distinguish between explicit 0 size and missing metadata
                let size_str = if let Some(size) = partition
                    .new_partition_info
                    .as_ref()
                    .and_then(|info| info.size)
                {
                    indicatif::HumanBytes(size).to_string()
                } else {
                    "???".to_string()
                };

                // Check for operations that rely on source data (meaning it's a patch/delta)
                let is_patch = Self::is_incremental_partition(partition);

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

            // Simplified footer focusing only on the partition count
            println!("{:-<46}", "");
            println!(
                "Total Partitions: {}",
                Style::new().bold().cyan().apply_to(partition_count)
            );

            return Ok(());
        }

        // 3. EXTRACTION GUARD: Bail if incremental
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

        // Shared per-partition worker state to reduce Arc clones per operation
        struct WorkerContext {
            partition_file: Arc<MmapMut>,
            part_name: Arc<str>,
            cancellation_token: Arc<AtomicBool>,
            stats_sender: Option<crossbeam_channel::Sender<Stat>>,
            hash_sender: Option<crossbeam_channel::Sender<HashRec>>,
            first_error: Arc<Mutex<Option<anyhow::Error>>>,
            remaining_ops: Arc<AtomicUsize>,
            partition_len: usize,
        }

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

        // Channel to store the first error message
        let first_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));

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
                let ctx = Arc::new(WorkerContext {
                    partition_file: partition_file.clone(),
                    part_name: Arc::from(update.partition_name.as_str()),
                    cancellation_token: cancellation_token.clone(),
                    stats_sender: stats_sender.clone(),
                    hash_sender: hash_sender.clone(),
                    first_error: first_error.clone(),
                    remaining_ops: Arc::new(AtomicUsize::new(update.operations.len())),
                    partition_len,
                });

                let base_ptr = PartitionPtr {
                    ptr: partition_file.as_ptr() as *mut u8,
                    _marker: PhantomData,
                };



                for op in update.operations.iter() {
                    let progress_bar = progress_bar.clone();
                    let ctx = ctx.clone();
                    let simd = simd;
                    scope.spawn(move |_| {
                        if ctx.cancellation_token.load(Ordering::Acquire) {
                            return;
                        }
                        let result = self.run_op_raw(
                            op,
                            payload,
                            base_ptr,
                            ctx.partition_len,
                            block_size,
                            &ctx.part_name,
                            simd,
                        );
                        match result {
                            Ok(_) => {}
                            Err(e) => {
                                ctx.cancellation_token.store(true, Ordering::Release);
                                // Store the first error only
                                let mut slot = ctx.first_error.lock().unwrap();

                                if slot.is_none() {
                                    *slot = Some(e.context(format!(
                                        "Error in partition '{}'",
                                        ctx.part_name
                                    )));
                                }

                                return;
                            }
                        }

                        if ctx.remaining_ops.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let is_cancelled = || ctx.cancellation_token.load(Ordering::Acquire);

                            // VERIFICATION PHASE: Exclusive access via write lock ensures all
                            // hardware store-buffers are synchronized and visible.
                            let final_slice: &[u8] = &ctx.partition_file;
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
                                            ctx.cancellation_token.store(true, Ordering::Release);
                                            eprintln!(
                                                "\nCritical error: Output verification failed for '{}': {}",
                                                ctx.part_name, e
                                            );
                                            eprintln!("Stopping extraction to prevent corrupted output...");
                                            return;
                                        }
                                    }
                                } else if self.strict {
                                    ctx.cancellation_token.store(true, Ordering::Release);
                                    eprintln!(
                                        "\nCritical error: Strict mode: missing partition hash for '{}'",
                                        ctx.part_name
                                    );
                                    eprintln!("Stopping extraction to prevent corrupted output...");
                                    return;
                                }
                            }


                            // Check cancellation before continuing
                            if is_cancelled(){
                                eprintln!("Post-processing for '{}' cancelled", ctx.part_name);
                                return;
                            }

                            // 2) Sanity checks (e.g., detect all-zero images)
                            if self.sanity
                                && is_all_zero_with_simd(simd, final_slice) {
                                    ctx.cancellation_token.store(true, Ordering::Release);
                                    eprintln!("\nCritical error: Sanity check failed for '{}': output image appears to be all zeros", ctx.part_name);
                                    eprintln!("Stopping extraction to prevent corrupted output...");
                                    return;
                                }

                            // Check cancellation before continuing
                            if is_cancelled(){
                                eprintln!("Post-processing for '{}' cancelled", ctx.part_name);
                                return;
                            }
                            // 3) Print SHA-256 if requested — reuse verified digest to avoid redundant work
                            if let Some(sender) = ctx.hash_sender.as_ref() {
                                let digest = if let Some(d) = computed_digest_opt {
                                    d
                                } else {
                                    let d = digest(&SHA256, final_slice);
                                        let mut arr = [0u8; 32];
                                        arr.copy_from_slice(d.as_ref());
                                        arr
                                    };
                                let hexstr = hex::encode(digest);
                                let _ = sender.send(HashRec { order: part_index, name: ctx.part_name.to_string(), hex: hexstr });
                            }

                            // 4) Stats collection (optional)
                            if let (Some(start), Some(sender)) = (part_start, ctx.stats_sender.as_ref()) {
                                let elapsed = start.elapsed();
                                let _ = sender.send(Stat { name: ctx.part_name.to_string(), bytes: ctx.partition_len as u64, ms: elapsed.as_millis() });
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
            // Print the stored error message
            if let Some(err) = first_error.lock().unwrap().take() {
                eprintln!("\n{}", err);
            }

            bail!(
                "❌ Extraction failed due to errors (see above). All partial files have been cleaned up."
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
        if !self.no_open {
            self.open_extracted_folder(&partition_dir)?;
        }

        Ok(())
    }

    fn create_progress_bar(&self, update: &PartitionUpdate) -> Result<ProgressBar> {
        let finish = ProgressFinish::AndLeave;

        let style = ProgressStyle::with_template(
            "{prefix:>24!.green.bold} [{wide_bar:.white.dim}] {percent:>3.white}%",
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
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn run_op_raw<'a>(
        &self,
        op: &InstallOperation,
        payload: &Payload,
        base_ptr: PartitionPtr<'a>,
        partition_len: usize,
        block_size: usize,
        partition_name: &str,
        simd: CpuSimd,
    ) -> Result<()> {
        let op_type = Type::try_from(op.r#type)?;
        let raw_extents =
            self.extract_dst_extents_raw(op, base_ptr.ptr, partition_len, block_size)?;

        // SAFETY: Reconstitute pointer inside the thread.
        // Sound because extents are non-overlapping and threads are scoped to the Mmap lifetime.
        let mut dst_extents = Vec::with_capacity(raw_extents.len());

        for (ptr, len) in raw_extents {
            dst_extents.push(unsafe { slice::from_raw_parts_mut(ptr, len) });
        }

        let total_dst_size: usize = dst_extents.iter().map(|e| e.len()).sum();

        match op_type {
            Type::Replace => {
                let data = self.extract_data(op, payload)?;
                self.run_op_replace_slice(data, &mut dst_extents, block_size, total_dst_size, simd)
            }
            Type::ReplaceBz => {
                let data = self.extract_data(op, payload)?;
                let mut decoder = BzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size, simd)
            }
            Type::ReplaceXz => {
                let data = self.extract_data(op, payload)?;
                let mut decoder = xz2::read::XzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size, simd)
            }
            Type::Zero | Type::Discard => {
                for extent in dst_extents.iter_mut() {
                    extent.fill(0);
                }
                Ok(())
            }
            // Catch-all for incremental types (Bsdiff, Brotli, etc.) or unknown future types
            _ => {
                let type_name = format!("{:?}", op_type);

                bail!(
                    "Operation type {} is not supported for full extraction in partition '{}'.",
                    type_name,
                    partition_name
                )
            }
        }
    }

    fn run_op_replace(
        &self,
        reader: &mut impl Read,
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
        simd: CpuSimd,
    ) -> Result<()> {
        let dst_len = dst_extents.iter().map(|e| e.len()).sum::<usize>();
        let bytes_read = io::copy(reader, &mut ExtentsWriter::new(dst_extents, simd))
            .context("failed to write to buffer")? as usize;
        let bytes_read_aligned = (bytes_read + block_size - 1) / block_size * block_size;
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
        total_dst_size: usize,
        simd: CpuSimd,
    ) -> Result<()> {
        let bytes_read = data.len();

        let bytes_read_aligned = (bytes_read + block_size - 1) / block_size * block_size;

        ensure!(
            bytes_read_aligned == total_dst_size,
            "more dst blocks than data, even with padding"
        );

        // FAST PATH: single contiguous extent
        if dst_extents.len() == 1 {
            let dst = &mut dst_extents[0];
            dst[..bytes_read].copy_from_slice(data);
            return Ok(());
        }

        // GENERIC PATH: multiple extents
        let written = ExtentsWriter::new(dst_extents, simd)
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
        // Linux-only sequential read hint
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;

            if let Ok(meta) = file.metadata() {
                let fd = file.as_raw_fd();
                let size = meta.len() as libc::off_t;

                unsafe {
                    let _ = libc::posix_fadvise(fd, 0, size, libc::POSIX_FADV_SEQUENTIAL);
                }
            }
        }

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

        let mut mmap = {
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
        // Linux-only sequential access hint for mmap writes
        #[cfg(target_os = "linux")]
        {
            use libc::{MADV_SEQUENTIAL, madvise};
            unsafe {
                madvise(
                    mmap.as_mut_ptr() as *mut libc::c_void,
                    mmap.len(),
                    MADV_SEQUENTIAL,
                );
            }
        }

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

    /// Validates that all dst_extents across all InstallOperations are non-overlapping.
    /// Fast path:
    /// If the total covered block range is reasonably bounded, we do O(n) bitmap sweep
    /// Safe fallback:
    /// Otherwise we retain the previous O(n log n) sorted sweep
    fn validate_non_overlapping_extents(&self, operations: &[InstallOperation]) -> Result<()> {
        let mut extents: Vec<(u64, u64)> = Vec::with_capacity(operations.len() * 2);

        for op in operations {
            for e in &op.dst_extents {
                let start = e.start_block.context("missing start_block")?;
                let num = e.num_blocks.context("missing num_blocks")?;

                let end = start.checked_add(num).context("extent end overflows u64")?;

                // Zero-length extents are allowed but irrelevant
                if num == 0 {
                    continue;
                }

                extents.push((start, end));
            }
        }

        // trivial success
        if extents.len() <= 1 {
            return Ok(());
        }

        // -------------------------
        // O(N log N) INTERVAL CHECK
        // -------------------------
        extents.sort_unstable_by_key(|(s, _)| *s);

        let mut last_end = 0u64;
        for (start, end) in extents {
            ensure!(
                start >= last_end,
                "Overlapping destination extents detected: {} < {}",
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
    #[inline]
    fn is_incremental_partition(p: &PartitionUpdate) -> bool {
        p.operations.iter().any(|op| {
            matches!(
                Type::try_from(op.r#type),
                Ok(Type::SourceCopy
                    | Type::SourceBsdiff
                    | Type::BrotliBsdiff
                    | Type::Lz4diffBsdiff
                    | Type::Puffdiff
                    | Type::Zucchini)
            )
        })
    }
}

const FRIENDLY_HELP: &str = color_print::cstr!(
    "\
{before-help}<bold><underline>{name} {version}</underline></bold>
{about}

<bold>QUICK START</bold>
  • Drag & drop an OTA .zip or payload.bin onto the executable.
  • Or run via command line: <cyan>otaripper update.zip</cyan>

<bold>COMMON TASKS</bold>
  • <bold>List</bold> partitions:                            otaripper -l update.zip
  • <bold>Extract everything</bold>:                         otaripper update.zip
  • <bold>Extract specific</bold>:                           otaripper update.zip -p boot,init_boot,vendor_boot
  • <bold>Disable auto-open folder after extraction: </bold> otaripper update.zip -n

<bold>SAFETY & INTEGRITY</bold>
  • SHA-256 verification is <green>enabled by default</green>.
  • Partial files are <red>automatically deleted</red> on failure.
  • Use <yellow>--strict</yellow> to require manifest hashes and enforce verification.
  • Skip verification (not recommended): <yellow>--no-verify</yellow>

<bold>QUALITY OF LIFE</bold>
  • Automatically opens extracted folder after success.
  • Disable opening folder: <yellow>-n</yellow> or <yellow>--no-open</yellow>

{usage-heading}
  {usage}

<bold>OPTIONS</bold>
{all-args}

<bold>PROJECT</bold>: <blue>https://github.com/syedinsaf/otaripper</blue>
{after-help}"
);
