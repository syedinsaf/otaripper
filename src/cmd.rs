use crate::chromeos_update_engine::install_operation::Type;
use crate::chromeos_update_engine::{DeltaArchiveManifest, InstallOperation, PartitionUpdate};
use crate::payload::Payload;
use anyhow::{Context, Result, bail, ensure};
use bzip2::read::BzDecoder;
use chrono::Utc;
use clap::{Parser, ValueHint};
use console::Style;
use crossbeam_channel::unbounded;
use ctrlc;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressFinish, ProgressStyle};
use memmap2::{Mmap, MmapMut};
use prost::Message;
use rayon::{ThreadPool, ThreadPoolBuilder};
use sha2::{Digest, Sha256};
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::convert::TryFrom;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::{Deref, Div, Mul};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use std::{env, slice};
use zip::ZipArchive;
use zip::result::ZipError;

const INLINE_HASHING_THRESHOLD: usize = 256 * 1024 * 1024; // 256 MiB threshold for inline hashing
const OPTIMAL_CHUNK_SIZE: usize = 64 * 1024; // 64KB chunk size for cache-friendly copying
const SIMD_THRESHOLD: usize = 1024; // Use SIMD for copies >= 1KB
const PROGRESS_UPDATE_FREQUENCY_HIGH: u8 = 2; // Hz for progress updates when partition count <= 32
const PROGRESS_UPDATE_FREQUENCY_LOW: u8 = 1; // Hz for progress updates when partition count > 32

const _HELP_TEMPLATE: &str = color_print::cstr!(
    "\
{before-help}<bold><underline>{name} {version}</underline></bold>
{author}
https://github.com/syedinsaf/otaripper

{about}

{usage-heading}
{tab}{usage}

{all-args}{after-help}"
);

#[derive(Debug, Parser)]
#[clap(
    about,
    author,
    disable_help_subcommand = true,
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

    /// Run lightweight plausibility checks on output images (e.g., detect all-zero images)
    #[clap(
        long,
        help = "Run quick sanity checks on output images and fail on obviously invalid content (e.g., all zeros)."
    )]
    plausibility_checks: bool,

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
}

// The Deref trait allows PayloadSource to be treated like a byte slice `&[u8]`,
// making its use seamless with the existing parsing logic.
impl Deref for PayloadSource {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            PayloadSource::Mapped(mmap) => mmap,
            PayloadSource::Owned(vec) => vec,
        }
    }
}

/// Merge contiguous output slices to reduce copy operations
///
/// This function takes a vector of mutable byte slices and merges adjacent ones
/// that are contiguous in memory. This optimization reduces the number of copy
/// operations needed when writing data to multiple extents.
///
/// # Safety
/// Merges adjacent slices from the same memory buffer.
/// # Arguments
///
/// * `extents` - A vector of mutable byte slices to be coalesced
fn coalesce_extents(extents: &mut Vec<&mut [u8]>) {
    if extents.is_empty() {
        return;
    }
    // Merge adjacent slices to reduce copy operations
    let mut tmp: Vec<&mut [u8]> = Vec::with_capacity(extents.len());
    tmp.extend(extents.drain(..));

    let mut out: Vec<&mut [u8]> = Vec::with_capacity(tmp.len());
    let mut cur = tmp.remove(0);

    for nxt in tmp {
        let cur_end = cur.as_ptr() as usize + cur.len();
        let nxt_start = nxt.as_ptr() as usize;
        if cur_end == nxt_start {
            let new_len = cur.len() + nxt.len();
            let start_ptr = cur.as_mut_ptr();

            // Safety: We know both slices are valid and adjacent within the same buffer
            // The new slice will be within the bounds of the original buffer
            cur = unsafe {
                // Verify the new slice doesn't exceed the original buffer bounds
                let original_end = start_ptr.add(new_len);
                if original_end <= start_ptr.add(cur.len() + nxt.len()) {
                    core::slice::from_raw_parts_mut(start_ptr, new_len)
                } else {
                    // Fallback to separate slices if bounds check fails
                    out.push(cur);
                    cur = nxt;
                    continue;
                }
            };
        } else {
            out.push(cur);
            cur = nxt;
        }
    }
    out.push(cur);
    *extents = out;
}

/// Writes data across multiple memory regions efficiently with optional hashing.
pub struct ExtentsWriter<'a> {
    extents: &'a mut [&'a mut [u8]],
    idx: usize,
    off: usize,
    /// Optional hasher for on-the-fly hashing while writing.
    hasher: Option<Sha256>,
    /// Total bytes written (for diagnostics/validation)
    total_written: usize,
}

impl<'a> ExtentsWriter<'a> {
    /// Create a new ExtentsWriter without hashing
    pub fn new(extents: &'a mut [&'a mut [u8]]) -> Self {
        Self {
            extents,
            idx: 0,
            off: 0,
            hasher: None,
            total_written: 0,
        }
    }

    /// Create a writer that also computes SHA-256 of all bytes written
    pub fn new_with_hasher(extents: &'a mut [&'a mut [u8]]) -> Self {
        Self {
            extents,
            idx: 0,
            off: 0,
            hasher: Some(Sha256::new()),
            total_written: 0,
        }
    }

    /// Finalize and return the computed SHA-256 if hashing was enabled
    pub fn finalize_hash(mut self) -> Option<[u8; 32]> {
        self.hasher.take().map(|h| {
            let out = h.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&out);
            arr
        })
    }

    /// Get the total number of bytes written
    #[allow(dead_code)]
    pub fn bytes_written(&self) -> usize {
        self.total_written
    }
    #[inline]
    fn advance_to_available_extent(&mut self) {
        while self.idx < self.extents.len() && self.off >= self.extents[self.idx].len() {
            self.idx += 1;
            self.off = 0;
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
    #[inline]
    fn write_to_current_extent(&mut self, data: &[u8]) -> usize {
        let available = self.current_extent_capacity();
        if available == 0 {
            return 0;
        }

        let to_copy = available.min(data.len());
        if to_copy == 0 {
            return 0;
        }

        // Safety check: ensure we don't exceed the extent bounds
        if self.idx >= self.extents.len() || self.off >= self.extents[self.idx].len() {
            return 0;
        }

        let extent = &mut self.extents[self.idx];

        // Additional bounds check for the extent slice
        if self.off + to_copy > extent.len() {
            return 0;
        }

        let dest_slice = &mut extent[self.off..self.off + to_copy];
        let src_slice = &data[..to_copy];

        if let Some(ref mut hasher) = self.hasher {
            hasher.update(src_slice);
        }

        // SIMD-optimized copying strategies based on size
        match to_copy {
            // Very small copies: direct slice assignment (compiler optimizes well)
            1..=8 => {
                dest_slice.copy_from_slice(src_slice);
            }
            // Medium copies: use copy_from_slice (LLVM optimizes to memcpy)
            9..=SIMD_THRESHOLD => {
                dest_slice.copy_from_slice(src_slice);
            }
            // Large copies: use SIMD-optimized copying
            _ => {
                simd_copy_large(src_slice, dest_slice);
            }
        }

        self.off += to_copy;
        self.total_written += to_copy;

        if self.off >= extent.len() {
            self.idx += 1;
            self.off = 0;
        }

        to_copy
    }
}

impl<'a> io::Write for ExtentsWriter<'a> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut total_written = 0;

        // Skip extents that are already full
        self.advance_to_available_extent();

        // Write to available extents
        while !buf.is_empty() && self.has_capacity() {
            let written = self.write_to_current_extent(buf);
            if written == 0 {
                // This shouldn't happen if has_capacity() is true, but let's be safe
                break;
            }

            total_written += written;
            buf = &buf[written..];

            // Advance to next extent if needed
            if !buf.is_empty() {
                self.advance_to_available_extent();
            }
        }

        Ok(total_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// SIMD detection enum
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

#[inline]
fn simd_copy_chunk(src: &[u8], dst: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        match CpuSimd::get() {
            CpuSimd::Avx512 => unsafe { simd_copy_avx512(src, dst) },
            CpuSimd::Avx2 => unsafe { simd_copy_avx2(src, dst) },
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
#[inline]
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

    // Handle remaining bytes with scalar copy
    if i < len {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
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

    // Handle remaining bytes with scalar copy
    if i < len {
        let remaining_src = &src[i..];
        let remaining_dst = &mut dst[i..];
        remaining_dst.copy_from_slice(remaining_src);
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

    // Handle remaining bytes with scalar copy
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
    let len = data.len();
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
        let block_size = manifest.block_size.context("block_size not defined")? as usize;

        // Allow a hidden override for inline hashing: OTARIPPER_INLINE=on|off|auto
        let env_inline = std::env::var("OTARIPPER_INLINE")
            .ok()
            .map(|s| s.to_lowercase());

        if self.list {
            manifest
                .partitions
                .sort_unstable_by(|p1, p2| p1.partition_name.cmp(&p2.partition_name));
            for partition in &manifest.partitions {
                let size = partition
                    .new_partition_info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(|size| indicatif::HumanBytes(size).to_string());
                let size = size.as_deref().unwrap_or("???");

                let bold_green = Style::new().bold().green();
                println!(
                    "{} ({size})",
                    bold_green.apply_to(&partition.partition_name)
                );
            }
            return Ok(());
        }

        for partition in &self.partitions {
            if !manifest
                .partitions
                .iter()
                .any(|p| &p.partition_name == partition)
            {
                bail!("partition \"{}\" not found in manifest", partition);
            }
        }

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
        let partition_dir = partition_dir.as_ref();

        let cleanup_state = Arc::new(Mutex::new((
            Vec::<PathBuf>::new(),
            partition_dir.to_path_buf(),
            created_new_dir,
        )));

        let cancellation_token = Arc::new(AtomicBool::new(false));

        let cleanup_state_ctrlc = Arc::clone(&cleanup_state);
        let cancellation_token_ctrlc = Arc::clone(&cancellation_token);

        ctrlc::set_handler(move || {
            eprintln!("\n\n Received interrupt signal (Ctrl+C). Cleaning up and exiting...");

            // Signal all worker threads to stop
            cancellation_token_ctrlc.store(true, Ordering::Release);
            std::thread::sleep(std::time::Duration::from_millis(100));
            // Perform cleanup
            if let Ok(state) = cleanup_state_ctrlc.lock() {
                let (files, dir, dir_is_new) = &*state;
                if !files.is_empty() {
                    eprintln!("Removing {} partially extracted files...", files.len());
                    let mut removed_files = 0;
                    for file in files {
                        if file.exists() {
                            if let Err(e) = fs::remove_file(file) {
                                eprintln!("Failed to remove {}: {}", file.display(), e);
                            } else {
                                removed_files += 1;
                            }
                        }
                    }
                    if removed_files > 0 {
                        eprintln!("Cleaned up {} partially extracted files", removed_files);
                    }
                }
                // If we created the directory, remove it entirely
                if *dir_is_new && dir.exists() {
                    if let Err(e) = fs::remove_dir_all(dir) {
                        eprintln!("Failed to remove directory {}: {}", dir.display(), e);
                    } else {
                        eprintln!("Removed extraction directory: {}", dir.display());
                    }
                }
            }

            eprintln!("✨ Cleanup completed. Goodbye!");
            std::process::exit(130); // Standard exit code for Ctrl+C (128 + SIGINT)
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
        if let Some(t) = self.threads {
            if t > 0 {
                eprintln!(
                    "Using {} worker thread(s)",
                    threadpool.current_num_threads()
                );
            }
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
            let mut hash_index_counter: usize = 0;
            for update in manifest.partitions.iter().filter(|update| {
                self.partitions.is_empty() || self.partitions.contains(&update.partition_name)
            }) {
                if cancellation_token.load(Ordering::Acquire) {
                    eprintln!("Extraction cancelled before processing '{}'", update.partition_name);
                    break;
                }
                let progress_bar = self.create_progress_bar(update)?;
                let progress_bar = multiprogress.add(progress_bar);
                let (partition_file, partition_len, out_path) =
                    self.open_partition_file(update, partition_dir)?;
                // Track the file we just created for cleanup in case of errors
                if let Ok(mut state) = cleanup_state.lock() {
                    state.0.push(out_path.clone());
                }

                // Stats start for this partition (optional)
                let part_start = if self.stats { Some(Instant::now()) } else { None };
                let stats_sender = stats_sender.clone();

                // Assign an order index for hash printing
                let part_index = hash_index_counter;
                hash_index_counter += 1;
                let hash_sender = hash_sender.clone();

                let remaining_ops = Arc::new(AtomicUsize::new(update.operations.len()));
                let inline_digest: Arc<Mutex<Option<[u8;32]>>> = Arc::new(Mutex::new(None));

                // Silent heuristic: enable inline hashing for large partitions to avoid a post-pass.
                let heuristic = partition_len >= INLINE_HASHING_THRESHOLD;
                // Default to OFF (post-pass). Allow env overrides:
                // OTARIPPER_INLINE=on  -> force inline on
                // OTARIPPER_INLINE=off -> force inline off
                // OTARIPPER_INLINE=auto -> use size heuristic (256 MiB)
                let inline_enabled = match env_inline.as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    Some("auto") => heuristic,
                    None => false,
                    _ => false,
                };

                for op in update.operations.iter() {
                    let progress_bar = progress_bar.clone();
                    let partition_file = Arc::clone(&partition_file);
                    let remaining_ops = Arc::clone(&remaining_ops);

                    let part_name = update.partition_name.clone();
                    let part_start = part_start;
                    let stats_sender = stats_sender.clone();
                    let partition_len_for_stats = partition_len;
                    let part_index = part_index;
                    let hash_sender = hash_sender.clone();
                    let inline_digest = inline_digest.clone();
                    let cancellation_token = Arc::clone(&cancellation_token);
                    scope.spawn(move |_| {
                        if cancellation_token.load(Ordering::Acquire) {
                            return;
                        }
                        // SAFETY: Scoped threads ensure mmap stays alive, operations write to separate regions
                        let result = {
                            let mmap_guard = partition_file.read().expect("RwLock poisoned");
                            let partition_slice = unsafe {
                                slice::from_raw_parts_mut(
                                    mmap_guard.as_ptr() as *mut u8,
                                    mmap_guard.len()
                                )
                            };
                            self.run_op_safe(op, payload, partition_slice, block_size, inline_enabled)
                        };
                        match result {
                            Ok(maybe_digest) => {
                                if cancellation_token.load(Ordering::Acquire) {
                                    return;
                                }
                                if let Some(d) = maybe_digest {
                                    if let Ok(mut lock) = inline_digest.lock() {
                                        *lock = Some(d);
                                    }
                                }
                            }
                            Err(e) => {
                                // Set cancellation token to stop other threads
                                cancellation_token.store(true, Ordering::Release);
                                eprintln!("\nCritical error: Operation '{}' failed: {}", op.r#type, e);
                                eprintln!("Stopping extraction to prevent corrupted output...");
                                return;
                            }
                        }

                        if cancellation_token.load(Ordering::Acquire) {
                            return;
                        }

                        // If this is the last operation of the partition, run post-processing.
                        if remaining_ops.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let final_slice = {
                                let mmap_guard = partition_file.read().expect("RwLock poisoned");
                                // Convert to a slice for verification
                                unsafe {
                                    slice::from_raw_parts(
                                        mmap_guard.as_ptr(),
                                        mmap_guard.len()
                                    )
                                }
                            };

                            // 1) Verification when enabled and hash provided
                            // Also capture computed digest to reuse for printing (avoids a second pass)
                            // Prefer inline digest if any op computed it while writing.
                            let mut computed_digest_opt: Option<[u8; 32]> = None;
                            if let Ok(lock) = inline_digest.lock() {
                                if let Some(d) = *lock {
                                    computed_digest_opt = Some(d);
                                }
                            }
                            if computed_digest_opt.is_none() && !self.no_verify {
                                if let Some(hash) = update
                                    .new_partition_info
                                    .as_ref()
                                    .and_then(|info| info.hash.as_ref())
                                {
                                    match self.verify_sha256_returning(final_slice, hash) {
                                        Ok(d) => computed_digest_opt = Some(d),
                                        Err(e) => {
                                            cancellation_token.store(true, Ordering::Release);
                                            eprintln!("\nCritical error: Output verification failed for '{}': {}", part_name, e);
                                            eprintln!("Stopping extraction to prevent corrupted output...");
                                            return;
                                        }
                                    }
                                } else if self.strict {
                                    cancellation_token.store(true, Ordering::Release);
                                    eprintln!("\nCritical error: Strict mode: missing partition hash for '{}'", part_name);
                                    eprintln!("Stopping extraction to prevent corrupted output...");
                                    return;
                                }
                            }

                            // Check cancellation before continuing
                            if cancellation_token.load(Ordering::Acquire) {
                                eprintln!("Post-processing for '{}' cancelled", part_name);
                                return;
                            }

                            // 2) Plausibility checks (e.g., detect all-zero images)
                            if self.plausibility_checks {
                                if is_all_zero(final_slice) {
                                    cancellation_token.store(true, Ordering::Release);
                                    eprintln!("\nCritical error: Plausibility check failed for '{}': output image appears to be all zeros", part_name);
                                    eprintln!("Stopping extraction to prevent corrupted output...");
                                    return;
                                }
                            }

                            // Check cancellation before continuing
                            if cancellation_token.load(Ordering::Acquire) {
                                eprintln!("Post-processing for '{}' cancelled", part_name);
                                return;
                            }

                            // 3) Optional recording of SHA-256 for the partition (printed later to keep output clean)
                            if let Some(sender) = hash_sender.as_ref() {
                                let hexstr = if let Some(d) = computed_digest_opt {
                                    hex::encode(d)
                                } else {
                                    let digest = Sha256::digest(final_slice);
                                    hex::encode(digest)
                                };
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
        self.display_extracted_folder_size(partition_dir)?;

        // Automatically open the extracted folder (unless disabled)
        if !self.no_open_folder {
            self.open_extracted_folder(partition_dir)?;
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

    /// Processes an individual operation from the payload manifest with proper lifetime safety.
    ///
    /// This is a safe wrapper around the operation processing that takes a mutable slice
    /// instead of a raw pointer, ensuring proper lifetime management.
    fn run_op_safe(
        &self,
        op: &InstallOperation,
        payload: &Payload,
        partition_slice: &mut [u8],
        block_size: usize,
        inline_enabled: bool,
    ) -> Result<Option<[u8; 32]>> {
        let mut dst_extents = self
            .extract_dst_extents_safe(op, partition_slice, block_size)
            .context("error extracting dst_extents")?;

        match Type::try_from(op.r#type) {
            Ok(Type::Replace) => {
                let data = self
                    .extract_data(op, payload)
                    .context("error extracting data")?;
                self.run_op_replace_slice(data, &mut dst_extents, block_size, inline_enabled)
                    .context("error in REPLACE operation")
            }
            Ok(Type::ReplaceBz) => {
                let data = self
                    .extract_data(op, payload)
                    .context("error extracting data")?;
                let mut decoder = BzDecoder::new(data);
                // Streamed readers cannot reliably produce a full-partition inline digest,
                // so we fall back to no-op for inline digest (return None).
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size, inline_enabled)
                    .map(|_| None)
                    .context("error in REPLACE_BZ operation")
            }
            Ok(Type::ReplaceXz) => {
                let data = self
                    .extract_data(op, payload)
                    .context("error extracting data")?;
                let mut decoder = xz2::read::XzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size, inline_enabled)
                    .map(|_| None)
                    .context("error in REPLACE_XZ operation")
            }
            Ok(Type::Zero) => {
                // This is a no-op since the partition is already zeroed
                Ok(None)
            }
            Ok(Type::Discard) => {
                // Discard is similar to Zero - we just leave the blocks as they are (zeroed)
                Ok(None)
            }
            Ok(Type::SourceCopy) => {
                bail!("SOURCE_COPY operation is not supported in this version")
            }
            Ok(Type::SourceBsdiff) => {
                bail!("SOURCE_BSDIFF operation is not supported in this version")
            }
            Ok(Type::Puffdiff) => {
                bail!("PUFFDIFF operation is not supported in this version")
            }
            Ok(Type::BrotliBsdiff) => {
                bail!("BROTLI_BSDIFF operation is not supported in this version")
            }
            Ok(Type::Zucchini) => {
                bail!("ZUCCHINI operation is not supported in this version")
            }
            Ok(Type::Lz4diffBsdiff) => {
                bail!("LZ4DIFF_BSDIFF operation is not supported in this version")
            }
            Ok(Type::Lz4diffPuffdiff) => {
                bail!("LZ4DIFF_PUFFDIFF operation is not supported in this version")
            }
            Ok(op_type) => {
                bail!("Unsupported operation type: {:?}", op_type)
            }
            Err(e) => {
                bail!("Unrecognized operation type: {:?}. Error: {}", op.r#type, e)
            }
        }
    }

    fn run_op_replace(
        &self,
        reader: &mut impl Read,
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
        inline_enabled: bool,
    ) -> Result<Option<[u8; 32]>> {
        let mut v: Vec<&mut [u8]> = Vec::with_capacity(dst_extents.len());
        v.extend(dst_extents.iter_mut().map(|e| &mut **e));
        coalesce_extents(&mut v);
        let dst_len = v.iter().map(|e| e.len()).sum::<usize>();

        // Perform the actual write using the coalesced v
        if inline_enabled {
            let mut writer = ExtentsWriter::new_with_hasher(v.as_mut_slice());
            let bytes_read =
                io::copy(reader, &mut writer).context("failed to write to buffer")? as usize;

            // Efficient EOF check: attempt a small read to confirm no extra bytes remain
            let mut probe = [0u8; 1];
            let eof = matches!(reader.read(&mut probe), Ok(0));
            ensure!(eof, "read fewer bytes than expected");

            // Align number of bytes read to block size. The formula for alignment is:
            // ((operand + alignment - 1) / alignment) * alignment
            let bytes_read_aligned = (bytes_read + block_size - 1)
                .div(block_size)
                .mul(block_size);
            ensure!(
                bytes_read_aligned == dst_len,
                "more dst blocks than data, even with padding"
            );

            if let Some(d) = writer.finalize_hash() {
                return Ok(Some(d));
            }
            return Ok(None);
        } else {
            let mut writer = ExtentsWriter::new(v.as_mut_slice());
            let bytes_read =
                io::copy(reader, &mut writer).context("failed to write to buffer")? as usize;

            // Efficient EOF check: attempt a small read to confirm no extra bytes remain
            let mut probe = [0u8; 1];
            let eof = matches!(reader.read(&mut probe), Ok(0));
            ensure!(eof, "read fewer bytes than expected");

            // Align number of bytes read to block size. The formula for alignment is:
            // ((operand + alignment - 1) / alignment) * alignment
            let bytes_read_aligned = (bytes_read + block_size - 1)
                .div(block_size)
                .mul(block_size);
            ensure!(
                bytes_read_aligned == dst_len,
                "more dst blocks than data, even with padding"
            );

            return Ok(None);
        }
    }

    fn run_op_replace_slice(
        &self,
        data: &[u8],
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
        inline_enabled: bool,
    ) -> Result<Option<[u8; 32]>> {
        let bytes_read = data.len();
        // Build a local Vec to allow coalescing
        let mut v: Vec<&mut [u8]> = dst_extents.iter_mut().map(|e| &mut **e).collect();
        coalesce_extents(&mut v);
        let dst_len: usize = v.iter().map(|e| e.len()).sum();

        // If this operation writes the entire destination in one shot, we can compute
        // the SHA-256 inline while writing and return it to the caller to avoid a
        // separate post-pass.
        let bytes_read_aligned = (bytes_read + block_size - 1)
            .div(block_size)
            .mul(block_size);
        if bytes_read_aligned == dst_len {
            // Single-shot full-partition write: enable inline hashing only when allowed.
            if inline_enabled {
                let mut writer = ExtentsWriter::new_with_hasher(v.as_mut_slice());
                let written = writer.write(data).context("failed to write to buffer")?;
                ensure!(
                    written == bytes_read,
                    "failed to write all data to destination extents"
                );
                // finalize and return digest
                if let Some(d) = writer.finalize_hash() {
                    return Ok(Some(d));
                }
                return Ok(None);
            } else {
                let mut writer = ExtentsWriter::new(v.as_mut_slice());
                let written = writer.write(data).context("failed to write to buffer")?;
                ensure!(
                    written == bytes_read,
                    "failed to write all data to destination extents"
                );
                return Ok(None);
            }
        }

        // Fallback: no inline digest possible
        let mut writer = ExtentsWriter::new(v.as_mut_slice());
        let written = writer.write(data).context("failed to write to buffer")?;
        // Ensure all data was written
        ensure!(
            written == bytes_read,
            "failed to write all data to destination extents"
        );

        // Verify alignment rule matches destination length
        ensure!(
            bytes_read_aligned == dst_len,
            "more dst blocks than data, even with padding"
        );
        Ok(None)
    }

    /// In-memory zip handling: returns a `PayloadSource` enum. If the input is a zip
    /// file, `payload.bin` is extracted directly to memory instead of a temp file.
    fn open_payload_file(&self, path: &Path) -> Result<PayloadSource> {
        let file = File::open(path)
            .with_context(|| format!("unable to open file for reading: {path:?}"))?;

        // Attempt to open as a zip archive. If it fails with InvalidArchive,
        // we assume it's a raw payload.bin file.
        match ZipArchive::new(&file) {
            Ok(mut archive) => {
                let mut zipfile = archive
                    .by_name("payload.bin")
                    .context("could not find payload.bin file in archive")?;

                let mut buffer = Vec::with_capacity(zipfile.size() as usize);
                zipfile
                    .read_to_end(&mut buffer)
                    .context("failed to decompress payload.bin from archive")?;
                Ok(PayloadSource::Owned(buffer))
            }
            Err(ZipError::InvalidArchive(_)) => {
                // Not a zip file, so memory-map it directly.
                let mmap = unsafe { Mmap::map(&file) }
                    .with_context(|| format!("failed to mmap file: {path:?}"))?;
                Ok(PayloadSource::Mapped(mmap))
            }
            Err(e) => Err(e).context("failed to open zip archive"),
        }
    }

    fn open_partition_file(
        &self,
        update: &PartitionUpdate,
        partition_dir: impl AsRef<Path>,
    ) -> Result<(Arc<RwLock<MmapMut>>, usize, PathBuf)> {
        let partition_len = update
            .new_partition_info
            .as_ref()
            .and_then(|info| info.size)
            .context("unable to determine output file size")?;

        let filename = Path::new(&update.partition_name).with_extension("img");
        let path: PathBuf = partition_dir.as_ref().join(filename);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("unable to open file for writing: {path:?}"))?;
        file.set_len(partition_len)?;
        let mmap = unsafe { MmapMut::map_mut(&file) }
            .with_context(|| format!("failed to mmap file: {path:?}"))?;

        let partition = Arc::new(RwLock::new(mmap));
        Ok((partition, partition_len as usize, path))
    }

    fn extract_data<'a>(&self, op: &InstallOperation, payload: &'a Payload) -> Result<&'a [u8]> {
        let data_len = op.data_length.context("data_length not defined")? as usize;
        let data = {
            let offset = op.data_offset.context("data_offset not defined")? as usize;
            payload
                .data
                .get(offset..offset + data_len)
                .context("data offset exceeds payload size")?
        };
        match &op.data_sha256_hash {
            Some(hash) if !self.no_verify => {
                self.verify_sha256(data, hash)
                    .context("input verification failed")?;
            }
            _ => {}
        }
        Ok(data)
    }

    /// Extract destination extents with proper lifetime safety.
    ///
    /// This function now takes a mutable slice reference instead of a raw pointer,
    /// ensuring proper lifetime management and memory safety.
    fn extract_dst_extents_safe<'a>(
        &self,
        op: &InstallOperation,
        partition_slice: &'a mut [u8],
        block_size: usize,
    ) -> Result<Vec<&'a mut [u8]>> {
        let mut out: Vec<&'a mut [u8]> = Vec::with_capacity(op.dst_extents.len());
        let partition_len = partition_slice.len();

        // We need to split the slice into multiple mutable borrows
        // This is safe because each extent refers to non-overlapping regions
        let mut remaining_slice = partition_slice;
        let mut current_offset = 0;

        // Sort extents by start_block to process them in order
        let mut sorted_extents: Vec<_> = op.dst_extents.iter().enumerate().collect();
        sorted_extents.sort_by_key(|(_, extent)| extent.start_block);

        // Create a temporary vector to hold the results in the correct order
        let mut temp_results: Vec<(usize, &'a mut [u8])> = Vec::with_capacity(op.dst_extents.len());

        for (original_index, extent) in sorted_extents {
            let start_block = extent
                .start_block
                .context("start_block not defined in extent")?
                as usize;
            let num_blocks = extent
                .num_blocks
                .context("num_blocks not defined in extent")? as usize;

            let partition_offset = start_block * block_size;
            let extent_len = num_blocks * block_size;

            ensure!(
                partition_offset + extent_len <= partition_len,
                "extent exceeds partition size: offset {} + length {} > partition size {}",
                partition_offset,
                extent_len,
                partition_len
            );

            // Calculate the offset within the remaining slice
            let skip_bytes = partition_offset.saturating_sub(current_offset);
            ensure!(
                skip_bytes <= remaining_slice.len(),
                "invalid extent offset: skip_bytes {} > remaining slice length {}",
                skip_bytes,
                remaining_slice.len()
            );

            // Split off the bytes we need to skip
            if skip_bytes > 0 {
                let (_, rest) = remaining_slice.split_at_mut(skip_bytes);
                remaining_slice = rest;
                current_offset += skip_bytes;
            }

            // Ensure we have enough bytes for this extent
            ensure!(
                extent_len <= remaining_slice.len(),
                "not enough bytes for extent: need {} but only have {}",
                extent_len,
                remaining_slice.len()
            );

            // Split off this extent
            let (extent_slice, rest) = remaining_slice.split_at_mut(extent_len);
            remaining_slice = rest;
            current_offset += extent_len;

            temp_results.push((original_index, extent_slice));
        }

        // Sort results back to original order and extract the slices
        temp_results.sort_by_key(|(index, _)| *index);
        out.extend(temp_results.into_iter().map(|(_, slice)| slice));

        Ok(out)
    }

    fn verify_sha256(&self, data: &[u8], exp_hash: &[u8]) -> Result<()> {
        // Use the accelerated SHA-256 (with asm feature enabled in Cargo.toml)
        let got_hash = Sha256::digest(data);
        ensure!(
            got_hash.as_slice() == exp_hash,
            "hash mismatch: expected {}, got {}",
            hex::encode(exp_hash),
            hex::encode(got_hash.as_slice())
        );
        Ok(())
    }

    // Same as verify_sha256, but returns the computed digest on success so it can be reused.
    fn verify_sha256_returning(&self, data: &[u8], exp_hash: &[u8]) -> Result<[u8; 32]> {
        let got_hash = Sha256::digest(data);
        ensure!(
            got_hash.as_slice() == exp_hash,
            "hash mismatch: expected {}, got {}",
            hex::encode(exp_hash),
            hex::encode(got_hash.as_slice())
        );
        Ok(got_hash
            .as_slice()
            .try_into()
            .expect("sha256 digest must be 32 bytes"))
    }

    fn create_partition_dir(&self) -> Result<(Cow<'_, PathBuf>, bool)> {
        let dir: Cow<'_, PathBuf> = match &self.output_dir {
            Some(output_base) => {
                // When -o is specified, create a timestamped folder within that directory
                let now = Utc::now();
                let timestamp_folder = format!("{}", now.format("extracted_%Y%m%d_%H%M%S"));
                Cow::Owned(output_base.join(timestamp_folder))
            }
            None => {
                // When no -o is specified, create timestamped folder in current directory
                let now = Utc::now();
                let current_dir = env::current_dir().with_context(|| {
                    "Failed to determine current directory. Please specify --output-dir explicitly."
                })?;
                let filename = format!("{}", now.format("extracted_%Y%m%d_%H%M%S"));
                Cow::Owned(current_dir.join(filename))
            }
        };
        let existed = dir.as_ref().exists();
        fs::create_dir_all(dir.as_ref())
            .with_context(|| format!("could not create output directory: {dir:?}"))?;
        Ok((dir, !existed))
    }

    fn get_threadpool(&self) -> Result<ThreadPool> {
        let mut builder = ThreadPoolBuilder::new();
        if let Some(t) = self.threads {
            if t > 0 {
                builder = builder.num_threads(t);
            }
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
            // Try common file managers on Linux
            let file_managers = ["xdg-open", "nautilus", "dolphin", "thunar", "pcmanfm"];

            for manager in &file_managers {
                if let Ok(_) = Command::new(manager).arg(dir_path).spawn() {
                    return Ok(());
                }
            }

            eprintln!("Warning: No suitable file manager found to open folder");
        }

        Ok(())
    }
}

// Friendlier, task-oriented help template shown for -h/--help
const FRIENDLY_HELP: &str = color_print::cstr!(
    "\
{before-help}<bold><underline>{name} {version}</underline></bold>
{about}

Quick start:
  - Drag and drop your OTA .zip or payload.bin onto otaripper, or run:
    otaripper [path-to-ota.zip|payload.bin]

Common tasks:
  - List partitions only:
    otaripper -l [ota.zip]
  - Extract everything into a timestamped folder:
    otaripper [ota.zip]
  - Extract specific partition(s):
    otaripper [ota.zip] --partitions boot,init_boot
  - Choose output directory and threads:
    otaripper [ota.zip] -o out -t 8

Safety and integrity:
  - Verification is on by default (SHA-256).
  - Use --strict to require hashes; do NOT combine with --no-verify.
  - On any error, extraction stops and partial images are deleted.

Performance enhancements:
  - SIMD optimization automatically detects and uses AVX512/AVX2/SSE2 for data operations
  - Multi-threaded extraction with automatic CPU core detection

User experience:
  - Automatically opens extracted folder when complete (use --no-open-folder to disable)

{usage-heading}
{usage}

Options:
{all-args}

Project: https://github.com/syedinsaf/otaripper
{after-help}"
);
