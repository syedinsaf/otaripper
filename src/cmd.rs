use std::borrow::Cow;
use std::cmp::Reverse;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::{Div, Mul};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::convert::TryFrom;
use std::{env, slice};
use std::time::Instant;
use std::arch::x86_64::*;
// use faster::prelude::*;  // Commented out due to compatibility issues
use bzip2::read::BzDecoder;
use chrono::Utc;
use clap::{Parser, ValueHint};
use console::Style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressFinish, ProgressStyle};
use memmap2::{Mmap, MmapMut};
use prost::Message;
use rayon::{ThreadPool, ThreadPoolBuilder};
use sha2::{Digest, Sha256};
use sync_unsafe_cell::SyncUnsafeCell;
use zip::result::ZipError;
use zip::ZipArchive;
use anyhow::{bail, ensure, Context, Result};


use crate::chromeos_update_engine::install_operation::Type;
use crate::chromeos_update_engine::{DeltaArchiveManifest, InstallOperation, PartitionUpdate};
use crate::payload::Payload;

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
    #[clap(long, help = "Require manifest hashes for partitions and operations; enforce verification and fail if any required hash is missing.")]
    strict: bool,

    /// Compute and print SHA-256 of each extracted partition image
    #[clap(long, help = "Compute and print the SHA-256 of each extracted partition image. If the manifest lacks a hash, this may add one linear pass over the image.")]
    print_hash: bool,

    /// Run lightweight plausibility checks on output images (e.g., detect all-zero images)
    #[clap(long, help = "Run quick sanity checks on output images and fail on obviously invalid content (e.g., all zeros).")]
    plausibility_checks: bool,

    /// Print per-partition and total timing/throughput statistics after extraction
    #[clap(long, help = "Print per-partition and total timing/throughput statistics after extraction.")]
    stats: bool,

    /// Don't automatically open the extracted folder after completion
    #[clap(long, help = "Don't automatically open the extracted folder after completion.")]
    no_open_folder: bool,

    /// Positional argument for the payload file
    #[clap(value_hint = ValueHint::FilePath)]
    #[clap(index = 1, value_name = "PATH")]
    positional_payload: Option<PathBuf>,
}

// Merge contiguous output slices to reduce copy operations
fn coalesce_extents(extents: &mut Vec<&mut [u8]>) {
    if extents.is_empty() { return; }
    // Build a temporary vector to iterate and merge
    let mut tmp: Vec<&mut [u8]> = Vec::with_capacity(extents.len());
    tmp.extend(extents.drain(..));

    let mut out: Vec<&mut [u8]> = Vec::with_capacity(tmp.len());
    // Start with the first slice
    let mut cur = tmp.remove(0);

    for nxt in tmp {
        let cur_end = cur.as_ptr() as usize + cur.len();
        let nxt_start = nxt.as_ptr() as usize;
        if cur_end == nxt_start {
            // Safety: both slices originate from the same mmap buffer and are adjacent.
            let new_len = cur.len() + nxt.len();
            let start_ptr = cur.as_mut_ptr();
            cur = unsafe { core::slice::from_raw_parts_mut(start_ptr, new_len) };
        } else {
            out.push(cur);
            cur = nxt;
        }
    }
    out.push(cur);
    *extents = out;
}

// Writer that spans multiple destination extents as a single continuous sink
struct ExtentsWriter<'a> {
    extents: &'a mut [&'a mut [u8]],
    idx: usize,
    off: usize,
}
impl<'a> ExtentsWriter<'a> { fn new(extents: &'a mut [&'a mut [u8]]) -> Self { Self { extents, idx: 0, off: 0 } } }
impl<'a> io::Write for ExtentsWriter<'a> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let mut written = 0usize;

        // Early return if there's nothing to write
        if buf.is_empty() {
            return Ok(0);
        }

        // Skip extents that are already full
        while self.idx < self.extents.len() && self.off >= self.extents[self.idx].len() {
            self.idx += 1;
            self.off = 0;
        }

        // Write to available extents
        while !buf.is_empty() && self.idx < self.extents.len() {
            let cur = &mut self.extents[self.idx];
            let room = cur.len().saturating_sub(self.off);

            if room == 0 {
                self.idx += 1;
                self.off = 0;
                continue;
            }

            let to_copy = room.min(buf.len());
            cur[self.off..self.off + to_copy].copy_from_slice(&buf[..to_copy]);
            self.off += to_copy;
            written += to_copy;
            buf = &buf[to_copy..];

            // Move to next extent if current one is full
            if self.off >= cur.len() {
                self.idx += 1;
                self.off = 0;
            }
        }

        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// Efficiently checks if a byte slice contains only zeros.
///
/// This function uses SIMD instructions to process multiple bytes at once,
/// providing significant performance improvements over scalar implementations.
/// It falls back to scalar processing for small slices or when SIMD is not available.
///
/// # Arguments
///
/// * `data` - The byte slice to check
///
/// # Returns
///
/// * `true` if all bytes in the slice are zero
/// * `false` if any byte in the slice is non-zero
#[inline]
fn is_all_zero(data: &[u8]) -> bool {
    let len = data.len();

    // Fast path for small slices
    if len < 32 {
        return data.iter().all(|&b| b == 0);
    }

    // Check if we can use SIMD instructions
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            return unsafe { is_all_zero_avx512(data) };
        } else if is_x86_feature_detected!("avx2") {
            return unsafe { is_all_zero_avx2(data) };
        } else if is_x86_feature_detected!("sse2") {
            return unsafe { is_all_zero_sse2(data) };
        }
    }

    // Fallback to scalar implementation
    data.iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn is_all_zero_avx512(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Process 64 bytes at a time with AVX512
    let mut i = 0;
    let simd_end = len - 63;
    
    while i < simd_end {
        let chunk = _mm512_loadu_si512(ptr.add(i) as *const __m512i);
        let zero = _mm512_setzero_si512();
        let cmp = _mm512_cmpeq_epi8_mask(chunk, zero);
        
        // If any byte is non-zero, mask will not be all ones
        if cmp != 0xffff_ffff_ffff_ffff {
            return false;
        }
        i += 64;
    }
    
    // Handle remaining bytes
    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn is_all_zero_avx2(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Process 32 bytes at a time with AVX2
    let mut i = 0;
    let simd_end = len - 31;
    
    while i < simd_end {
        let chunk = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let zero = _mm256_setzero_si256();
        let cmp = _mm256_cmpeq_epi8(chunk, zero);
        let mask = _mm256_movemask_epi8(cmp);
        
        // If any byte is non-zero, mask will not be -1 (all bits set)
        if mask != -1 {
            return false;
        }
        i += 32;
    }
    
    // Handle remaining bytes
    data[i..].iter().all(|&b| b == 0)
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn is_all_zero_sse2(data: &[u8]) -> bool {
    let len = data.len();
    let ptr = data.as_ptr();
    
    // Process 16 bytes at a time with SSE2
    let mut i = 0;
    let simd_end = len - 15;
    
    while i < simd_end {
        let chunk = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let zero = _mm_setzero_si128();
        let cmp = _mm_cmpeq_epi8(chunk, zero);
        let mask = _mm_movemask_epi8(cmp);
        
        // If any byte is non-zero, mask will not be -1 (all bits set)
        if mask != -1 {
            return false;
        }
        i += 16;
    }
    
    // Handle remaining bytes
    data[i..].iter().all(|&b| b == 0)
}

impl Cmd {
    pub fn run(&self) -> Result<()> {
        // Determine the payload path
        let payload_path = self
            .payload
            .clone()
            .or_else(|| self.positional_payload.clone())
            .ok_or_else(|| anyhow::anyhow!(
                "No payload file specified. Please provide a payload file using -p or as a positional argument."
            ))?;

        // Proceed with the rest of the method using payload_path
        let payload = self.open_payload_file(&payload_path)?;
        let payload = &Payload::parse(&payload)?;

        let mut manifest =
            DeltaArchiveManifest::decode(payload.manifest).context("unable to parse manifest")?;
        let block_size = manifest.block_size.context("block_size not defined")? as usize;

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
                println!("{} ({size})", bold_green.apply_to(&partition.partition_name));
            }
            return Ok(());
        }

        for partition in &self.partitions {
            if !manifest.partitions.iter().any(|p| &p.partition_name == partition) {
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
        let total_start = if self.stats { Some(Instant::now()) } else { None };
        #[derive(Clone)]
        struct Stat { name: String, bytes: u64, ms: u128 }
        let stats_records: Option<Arc<Mutex<Vec<Stat>>>> = if self.stats {
            Some(Arc::new(Mutex::new(Vec::new())))
        } else { None };

        // Optional hash records for clean printing after extraction
        #[derive(Clone)]
        struct HashRec { order: usize, name: String, hex: String }
        let hash_records: Option<Arc<Mutex<Vec<HashRec>>>> = if self.print_hash {
            Some(Arc::new(Mutex::new(Vec::new())))
        } else { None };

        // Count selected partitions for progress redraw heuristic
        let selected_count: usize = manifest
            .partitions
            .iter()
            .filter(|u| self.partitions.is_empty() || self.partitions.contains(&u.partition_name))
            .count();

        // Strict mode sanity: ensure hashes exist when required and disallow --no-verify with --strict
        if self.strict {
            if self.no_verify {
                bail!("--strict cannot be used together with --no-verify");
            }
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

        // Cleanup state: tracks files to delete and directory info for error cleanup

        let threadpool = self.get_threadpool()?;
        
        // Create a shared cleanup state that can be accessed from any thread
        let cleanup_state = Arc::new(Mutex::new((Vec::new(), partition_dir.to_path_buf(), created_new_dir)));
        
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
                eprintln!("Using {} worker thread(s)", threadpool.current_num_threads());
            }
        }
        threadpool.scope(|scope| -> Result<()> {
            let multiprogress = {
                // Setting a fixed update frequency reduces flickering.
                let hz = if selected_count > 32 { 1 } else { 2 };
                let draw_target = ProgressDrawTarget::stderr_with_hz(hz);
                MultiProgress::with_draw_target(draw_target)
            };
            // Maintain the manifest/extraction order for neatly printing hashes later
            let mut hash_index_counter: usize = 0;
            for update in manifest.partitions.iter().filter(|update| {
                self.partitions.is_empty() || self.partitions.contains(&update.partition_name)
            }) {
                let progress_bar = self.create_progress_bar(update)?;
                let progress_bar = multiprogress.add(progress_bar);
                let (partition_file, partition_len, out_path) =
                    self.open_partition_file(update, partition_dir)?;
                // Track the file we just created for cleanup in case of errors
                if let Ok(mut state) = cleanup_state.lock() {
                    state.0.push(out_path);
                }

                // Stats start for this partition (optional)
                let part_start = if self.stats { Some(Instant::now()) } else { None };
                let stats_records = stats_records.clone();

                // Assign an order index for hash printing
                let part_index = hash_index_counter;
                hash_index_counter += 1;
                let hash_records = hash_records.clone();

                let remaining_ops = Arc::new(AtomicUsize::new(update.operations.len()));

                for op in update.operations.iter() {
                    let progress_bar = progress_bar.clone();
                    let partition_file = Arc::clone(&partition_file);
                    let remaining_ops = Arc::clone(&remaining_ops);

                    let part_name = update.partition_name.clone();
                    let part_start = part_start;
                    let stats_records = stats_records.clone();
                    let partition_len_for_stats = partition_len;
                    let part_index = part_index;
                    let hash_records = hash_records.clone();
                    scope.spawn(move |_| {
                        let partition = unsafe { (*partition_file.get()).as_mut_ptr() };
                        if let Err(e) = self.run_op(op, payload, partition, partition_len, block_size) {
                            panic!("operation failed: {:#}", e);
                        }

                        // If this is the last operation of the partition, run post-processing.
                        if remaining_ops.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let final_view = unsafe { (*partition_file.get()).as_ref() };

                            // 1) Verification when enabled and hash provided
                            // Also capture computed digest to reuse for printing (avoids a second pass)
                            let mut computed_digest_opt: Option<[u8; 32]> = None;
                            if !self.no_verify {
                                if let Some(hash) = update
                                    .new_partition_info
                                    .as_ref()
                                    .and_then(|info| info.hash.as_ref())
                                {
                                    match self.verify_sha256_returning(final_view, hash) {
                                        Ok(d) => computed_digest_opt = Some(d),
                                        Err(e) => panic!("output verification failed: {:#}", e),
                                    }
                                } else if self.strict {
                                    panic!("strict mode: missing partition hash for '{}'", part_name);
                                }
                            }

                            // 2) Plausibility checks (e.g., detect all-zero images)
                            if self.plausibility_checks {
                                if is_all_zero(final_view) {
                                    panic!("plausibility check failed: output image '{}' appears to be all zeros", part_name);
                                }
                            }

                            // 3) Optional recording of SHA-256 for the partition (printed later to keep output clean)
                            if let Some(records) = hash_records.as_ref() {
                                let hexstr = if let Some(d) = computed_digest_opt {
                                    hex::encode(d)
                                } else {
                                    let digest = Sha256::digest(final_view);
                                    hex::encode(digest)
                                };
                                if let Ok(mut v) = records.lock() {
                                    v.push(HashRec { order: part_index, name: part_name.clone(), hex: hexstr });
                                }
                            }

                            // 4) Stats collection (optional)
                            if let (Some(start), Some(stats)) = (part_start, stats_records) {
                                let elapsed = start.elapsed();
                                if let Ok(mut v) = stats.lock() {
                                    v.push(Stat { name: part_name, bytes: partition_len_for_stats as u64, ms: elapsed.as_millis() });
                                }
                            }
                        }

                        progress_bar.inc(1);
                    });
                }
            }
            Ok(())
        })?;

        // Print partition hashes (cleanly) if requested
        if let Some(records) = hash_records.as_ref() {
            if let Ok(mut v) = records.lock() {
                if !v.is_empty() {
                    v.sort_by_key(|r| r.order);
                    println!("Partition hashes (SHA-256):");
                    for r in v.iter() {
                        println!("{}: sha256={}", r.name, r.hex);
                    }
                }
            }
        }

        // Print stats summary if requested
        if let Some(stats) = stats_records.as_ref() {
            if let Ok(v) = stats.lock() {
                if !v.is_empty() {
                    let total_bytes: u64 = v.iter().map(|s| s.bytes).sum();
                    let wall_ms = total_start.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                    eprintln!("\nExtraction statistics:");
                    for s in v.iter() {
                        let gbps = if s.ms > 0 { (s.bytes as f64) / (s.ms as f64) / 1_000_000.0 } else { 0.0 };
                        eprintln!("  - {}: {} in {} ms ({:.2} GB/s)", s.name, indicatif::HumanBytes(s.bytes), s.ms, gbps);
                    }
                    if wall_ms > 0 {
                        let total_gbps = (total_bytes as f64) / (wall_ms as f64) / 1_000_000.0;
                        eprintln!("  Total: {} in {} ms ({:.2} GB/s)", indicatif::HumanBytes(total_bytes), wall_ms, total_gbps);
                    } else {
                        eprintln!("  Total: {}", indicatif::HumanBytes(total_bytes));
                    }
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

    /// Processes an individual operation from the payload manifest.
    ///
    /// This method handles various operation types defined in the InstallOperation::Type enum:
    /// - REPLACE: Replace destination extents with attached data
    /// - REPLACE_BZ: Replace destination extents with attached bzipped data
    /// - REPLACE_XZ: Replace destination extents with attached xz data
    /// - ZERO: Write zeros in the destination (no-op since partition is pre-zeroed)
    /// - DISCARD: Discard the destination blocks
    /// - SOURCE_COPY: Copy from source to target partition (unsupported)
    /// - SOURCE_BSDIFF: Apply bsdiff from source partition (unsupported)
    /// - PUFFDIFF: Apply puffdiff format data (unsupported)
    /// - BROTLI_BSDIFF: Apply brotli-compressed bsdiff (unsupported)
    /// - ZUCCHINI: Apply zucchini format data (unsupported)
    /// - LZ4DIFF_BSDIFF: Apply lz4-compressed bsdiff (unsupported)
    /// - LZ4DIFF_PUFFDIFF: Apply lz4-compressed puffdiff (unsupported)
    ///
    /// # Arguments
    ///
    /// * `op` - The InstallOperation to process
    /// * `payload` - The payload containing the data for the operation
    /// * `partition` - Pointer to the partition data
    /// * `partition_len` - Length of the partition data
    /// * `block_size` - Block size for the partition
    ///
    /// # Returns
    ///
    /// * `Ok(())` if the operation was processed successfully
    /// * `Err` with a descriptive error message if the operation failed
    fn run_op(
        &self,
        op: &InstallOperation,
        payload: &Payload,
        partition: *mut u8,
        partition_len: usize,
        block_size: usize,
    ) -> Result<()> {
        let mut dst_extents = self
            .extract_dst_extents(op, partition, partition_len, block_size)
            .context("error extracting dst_extents")?;

        match Type::try_from(op.r#type) {
            Ok(Type::Replace) => {
                let data = self.extract_data(op, payload).context("error extracting data")?;
                self.run_op_replace_slice(data, &mut dst_extents, block_size)
                    .context("error in REPLACE operation")
            }
            Ok(Type::ReplaceBz) => {
                let data = self.extract_data(op, payload).context("error extracting data")?;
                let mut decoder = BzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size)
                    .context("error in REPLACE_BZ operation")
            }
            Ok(Type::ReplaceXz) => {
                let data = self.extract_data(op, payload).context("error extracting data")?;
                let mut decoder = xz2::read::XzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size)
                    .context("error in REPLACE_XZ operation")
            }
            Ok(Type::Zero) => {
                // This is a no-op since the partition is already zeroed
                Ok(())
            }
            Ok(Type::Discard) => {
                // Discard is similar to Zero - we just leave the blocks as they are (zeroed)
                Ok(())
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
    ) -> Result<()> {
        // Build a local Vec to allow coalescing and single-writer usage
        let mut v: Vec<&mut [u8]> = dst_extents.iter_mut().map(|e| &mut **e).collect();
        coalesce_extents(&mut v);
        // Compute total destination length
        let dst_len = v.iter().map(|e| e.len()).sum::<usize>();

        // Single copy over all extents via ExtentsWriter
        let mut writer = ExtentsWriter::new(v.as_mut_slice());
        let bytes_read = io::copy(reader, &mut writer).context("failed to write to buffer")? as usize;

        // Efficient EOF check: attempt a small read to confirm no extra bytes remain
        let mut probe = [0u8; 1];
        let eof = matches!(reader.read(&mut probe), Ok(0));
        ensure!(eof, "read fewer bytes than expected");

        // Align number of bytes read to block size. The formula for alignment is:
        // ((operand + alignment - 1) / alignment) * alignment
        let bytes_read_aligned = (bytes_read + block_size - 1).div(block_size).mul(block_size);
        ensure!(bytes_read_aligned == dst_len, "more dst blocks than data, even with padding");

        Ok(())
    }

    fn run_op_replace_slice(
        &self,
        data: &[u8],
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
    ) -> Result<()> {
        let bytes_read = data.len();
        // Build a local Vec to allow coalescing
        let mut v: Vec<&mut [u8]> = dst_extents.iter_mut().map(|e| &mut **e).collect();
        coalesce_extents(&mut v);
        let dst_len: usize = v.iter().map(|e| e.len()).sum();

        // Use ExtentsWriter for more efficient copying
        // This avoids the loop and multiple copy operations
        let mut writer = ExtentsWriter::new(v.as_mut_slice());
        let written = writer.write(data).context("failed to write to buffer")?;

        // Ensure all data was written
        ensure!(written == bytes_read, "failed to write all data to destination extents");

        // Verify alignment rule matches destination length
        let bytes_read_aligned = (bytes_read + block_size - 1).div(block_size).mul(block_size);
        ensure!(bytes_read_aligned == dst_len, "more dst blocks than data, even with padding");
        Ok(())
    }

    fn open_payload_file(&self, path: &Path) -> Result<Mmap> {
        let file = File::open(path)
            .with_context(|| format!("unable to open file for reading: {path:?}"))?;

        // Assume the file is a zip archive. If it's not, we get an
        // InvalidArchive error, and we can treat it as a payload.bin file.
        match ZipArchive::new(&file) {
            Ok(mut archive) => {
                // TODO: add progress indicator while zip file is being extracted.
                let zipfile = archive
                    .by_name("payload.bin")
                    .context("could not find payload.bin file in archive")?;

                let tmp = tempfile::tempfile().context("failed to create temporary file")?;
                let _ = tmp.set_len(zipfile.size());

                // Buffered copy for better throughput
                let mut reader = io::BufReader::with_capacity(1 << 20, zipfile); // 1 MiB
                let mut writer = io::BufWriter::with_capacity(1 << 20, tmp);
                io::copy(&mut reader, &mut writer).context("failed to write to temporary file")?;
                writer.flush().ok();
                let inner = writer
                    .into_inner()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                unsafe { Mmap::map(&inner) }.context("failed to mmap temporary file")
            }
            Err(ZipError::InvalidArchive(_)) => unsafe { Mmap::map(&file) }
                .with_context(|| format!("failed to mmap file: {path:?}")),
            Err(e) => Err(e).context("failed to open zip archive"),
        }
    }

    fn open_partition_file(
        &self,
        update: &PartitionUpdate,
        partition_dir: impl AsRef<Path>,
    ) -> Result<(Arc<SyncUnsafeCell<MmapMut>>, usize, PathBuf)> {
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

        let partition = Arc::new(SyncUnsafeCell::new(mmap));
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
                self.verify_sha256(data, hash).context("input verification failed")?;
            }
            _ => {}
        }
        Ok(data)
    }

    fn extract_dst_extents(
        &self,
        op: &InstallOperation,
        partition: *mut u8,
        partition_len: usize,
        block_size: usize,
    ) -> Result<Vec<&'static mut [u8]>> {
        let mut out: Vec<&'static mut [u8]> = Vec::with_capacity(op.dst_extents.len());
        for extent in &op.dst_extents {
            let start_block = extent.start_block.context("start_block not defined in extent")? as usize;
            let num_blocks = extent.num_blocks.context("num_blocks not defined in extent")? as usize;

            let partition_offset = start_block * block_size;
            let extent_len = num_blocks * block_size;

            ensure!(partition_offset + extent_len <= partition_len, "extent exceeds partition size");
            let slice_ref = unsafe { slice::from_raw_parts_mut(partition.add(partition_offset), extent_len) };
            out.push(slice_ref);
        }
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
        Ok(got_hash.as_slice().try_into().expect("sha256 digest must be 32 bytes"))
    }

    fn create_partition_dir(&self) -> Result<(Cow<'_, PathBuf>, bool)> {
        let dir = match &self.output_dir {
            Some(dir) => Cow::Borrowed(dir),
            None => {
                let now = Utc::now();
                let current_dir = env::current_dir().context("please specify --output-dir")?;
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
        builder
            .build()
            .context("unable to start threadpool")
    }

    /// Calculate and display the total size of the extracted folder
    fn display_extracted_folder_size(&self, partition_dir: impl AsRef<Path>) -> Result<()> {
        let dir_path = partition_dir.as_ref();
        
        // Calculate total size recursively
        let total_size = self.calculate_directory_size(dir_path)?;
        
        // Display the result
        println!("\nExtraction completed successfully!");
        println!("Output directory: {}", dir_path.display());
        println!("Total extracted size: {}", indicatif::HumanBytes(total_size));
        
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
                let entry = entry
                    .with_context(|| format!("failed to read directory entry in: {}", path.display()))?;
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

User experience:
  - Automatically opens extracted folder when complete (use --no-open-folder to disable)

{usage-heading}
{usage}

Options:
{all-args}

Project: https://github.com/syedinsaf/otaripper
{after-help}"
);
