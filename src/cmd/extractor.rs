use crate::payload::Payload;
use crate::proto::chromeos_update_engine::install_operation::Type;
use crate::proto::chromeos_update_engine::{
    DeltaArchiveManifest, InstallOperation, PartitionUpdate,
};
use anyhow::{Context, Result, bail, ensure};

use crate::cmd::SubCmd;
use bzip2::read::BzDecoder;
use chrono::Local;

use console::Style;
use crossbeam_channel::unbounded;
use ctrlc;
use indicatif::{MultiProgress, ProgressBar, ProgressFinish, ProgressStyle};
use memmap2::{Mmap, MmapMut};
use prost::Message;
use rayon::{ThreadPool, ThreadPoolBuilder};
use ring::digest::{SHA256, digest};
use std::cell::RefCell;
use std::cmp::Reverse;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{env, slice};
use sysinfo::{MemoryRefreshKind, RefreshKind};
use tempfile::NamedTempFile;
use zip::ZipArchive;

use super::simd::*;

// ===== Android OTA limits =====
const MIN_BLOCK_SIZE: usize = 512;
const MAX_BLOCK_SIZE: usize = 16 * 1024 * 1024;

// ===== Thread-local Buffers =====
thread_local! {
    /// 1MB buffer utilized by `run_op_replace` to amortize Rayon allocation costs
    /// and to ensure SIMD streaming (non-temporal writes) can trigger for decompressed payloads.
    static COPY_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; 1024 * 1024]);
}

pub enum PayloadSource {
    Mapped(Mmap),
    MappedOffset(Mmap, usize),
    Owned(Vec<u8>),
    Temp(Mmap, NamedTempFile),
    #[cfg(feature = "remote")]
    Remote {
        manifest_buf: Vec<u8>,
        url: String,
        payload_base_offset: u64,
        client: reqwest::blocking::Client,
    },
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct PartitionPtr(*mut u8);

// SAFETY:
// - Pointer comes from Arc<MmapMut>
// - validate_non_overlapping_extents guarantees no aliasing
// - rayon::scope prevents threads from outliving the mmap
unsafe impl Send for PartitionPtr {}
unsafe impl Sync for PartitionPtr {}

#[derive(Clone)]
struct Stat {
    name: String,
    bytes: u64,
    ms: u128,
}

// Optional hash records for clean printing after extraction
#[derive(Clone)]
struct HashRec {
    order: usize,
    name: String,
    hex: String,
}

// State for automatic file cleanup on interrupt or panic
struct CleanupState {
    files: Vec<PathBuf>,
    dir: PathBuf,
    dir_is_new: bool,
}

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

pub(super) struct Extractor<'a> {
    pub cmd: &'a super::Cmd,
}

impl<'a> Extractor<'a> {
    fn run_clean(&self, base_dir: Option<&Path>) -> Result<()> {
        let base_dir = match base_dir {
            Some(p) => p.to_path_buf(),
            None => env::current_dir().context("failed to determine current directory")?,
        };
        ensure!(
            base_dir
                .components()
                .any(|c| matches!(c, Component::Normal(_))),
            "Refusing to clean filesystem root."
        );

        println!("Scanning for extracted folders in:");
        println!("  {}", base_dir.display());

        let mut targets = Vec::<PathBuf>::new();

        for entry in fs::read_dir(&base_dir)? {
            let entry = entry?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            if name.starts_with("extracted_") {
                targets.push(path);
            }
        }

        if targets.is_empty() {
            println!("No extracted folders found.");
            return Ok(());
        }

        println!("\nThe following folders will be removed:");
        for dir in &targets {
            println!("  {}", dir.display());
        }

        println!("\nProceed? [Y/n]");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();

        if input != "y" && input != "yes" {
            println!("Aborted.");
            return Ok(());
        }

        for dir in targets {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to remove {}", dir.display()))?;
            println!("Removed {}", dir.display());
        }

        println!("\nCleanup complete.");
        Ok(())
    }
    // High-level extraction flow:
    // 1. Parse and validate payload
    // 2. Reject incremental OTAs
    // 3. Prepare output + threadpool
    // 4. Extract partitions in size-descending order
    // 5. Verify, sanity-check, and finalize output
    pub fn run(&self) -> Result<()> {
        // Handle subcommands early (before extraction logic)
        if let Some(subcmd) = &self.cmd.subcmd {
            match subcmd {
                SubCmd::Clean { output_dir } => {
                    return self.run_clean(output_dir.as_deref());
                }
                SubCmd::Arbscan { no_json, image } => {
                    return crate::cmd::arbscan::run(*no_json, image);
                }
            }
        }

        // Initialize SIMD detection early - this ensures SIMD capabilities are
        // detected and available for all operations throughout the extraction
        let simd = CpuSimd::get();
        if let Some(t) = self.cmd.threads {
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

        let payload_path = self.cmd.positional_payload.as_ref()
            .ok_or_else(|| anyhow::anyhow!(
                "No payload file or URL specified.\n\
        \n\
        Usage:\n\
          otaripper <payload.zip | payload.bin | HTTP URL>\n\
          otaripper arbscan <xbl_config.img>\n\
        \n\
        Examples:\n\
          • Stream an entire OTA directly from a URL:\n\
              otaripper https://example.com/rom.zip\n\
        \n\
          • Stream only specific partitions from a URL (massive bandwidth W):\n\
              otaripper https://example.com/rom.zip -p boot,init_boot\n\
        \n\
          • Extract a local file:\n\
              otaripper update.zip\n\
        \n\
          • Extract only specific partitions:\n\
              otaripper update.zip -p boot,init_boot,vendor_boot\n\
        \n\
          • Scan bootloader for ARB metadata:\n\
              otaripper arbscan xbl_config.img\n\
        \n\
        Tip:\n\
          Use comma-separated partition names without .img extension. Names must match the OTA manifest.\n\
        \n\
        For more options and features, run:\n\
          otaripper -h\n"
            ))?
            .clone();

        if payload_path.is_dir() {
            if self.cmd.fff {
                return self.reorganize_extracted_dir(&payload_path);
            } else {
                bail!(
                    "\"{}\" is a directory. Extraction is not possible.\n\n\
                     Hint: If you want to reorganize an already extracted folder for Fastboot Firmware Flasher, use the --fff flag:\n\
                     otaripper {} --fff",
                    payload_path.display(),
                    payload_path.display()
                );
            }
        }

        let payload_path_str = payload_path.to_str().context("Path is not valid UTF-8")?;

        let metadata = crate::cmd::metadata::fetch_metadata(payload_path_str);
        if let Some(ref meta) = metadata {
            if !self.cmd.quiet {
                println!("Firmware Information");
                println!("────────────────────");

                // 1. Device Model
                if let Some(model) = meta.get("product_name").or(meta.get("pre-device")) {
                    println!("{:<26} : {}", "Product Model", model);
                }

                // 2. Android Version
                if let Some(android_ver) = meta.get("android_version") {
                    println!("{:<26} : {}", "Android Version", android_ver);
                }

                // 3. Build/OS Version
                if let Some(build_ver) = meta.get("version_name").or(meta.get("post-build")) {
                    println!("{:<26} : {}", "Build Version", build_ver);
                }

                // 4. ColorOS/OxygenOS Version
                if let Some(rom_ver) = meta.get("oplus_rom_version").or(meta.get("os_version")) {
                    // Avoid duplicating version_name if os_version happens to equal version_name
                    if let Some(version_name) = meta.get("version_name") {
                        if rom_ver != version_name {
                            println!("{:<26} : {}", "OxygenOS/ColorOS Version", rom_ver);
                        }
                    } else {
                        println!("{:<26} : {}", "OxygenOS/ColorOS Version", rom_ver);
                    }
                }

                // 5. Security Patch
                if let Some(patch) = meta.get("security_patch") {
                    println!("{:<26} : {}", "Security Patch", patch);
                }

                println!();
            }
        }

        let payload_source = self.open_payload_file(payload_path_str)?;
        let payload_parsed = match &payload_source {
            #[cfg(feature = "remote")]
            PayloadSource::Remote {
                manifest_buf,
                url,
                payload_base_offset,
                client,
            } => Payload::parse_remote(
                manifest_buf,
                url.clone(),
                *payload_base_offset,
                client.clone(),
            )?,
            local_source => {
                let bytes: &[u8] = match local_source {
                    PayloadSource::Mapped(m) => m,
                    PayloadSource::MappedOffset(m, offset) => &m[*offset..],
                    PayloadSource::Owned(v) => v,
                    PayloadSource::Temp(m, _) => m,
                    #[cfg(feature = "remote")]
                    PayloadSource::Remote { .. } => unreachable!(),
                };
                Payload::parse(bytes)?
            }
        };
        let payload = &payload_parsed;

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
        if self.cmd.list {
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
        for partition in &self.cmd.partitions {
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
        let total_start = if self.cmd.stats {
            Some(Instant::now())
        } else {
            None
        };

        // Use channels to minimize contention: workers send Stat structs to a receiver
        let (stats_sender, stats_receiver) = if self.cmd.stats {
            let (s, r) = unbounded::<Stat>();
            (Some(s), Some(r))
        } else {
            (None, None)
        };

        // Channel for hash records
        let (hash_sender, hash_receiver) = if self.cmd.print_hash {
            let (s, r) = unbounded::<HashRec>();
            (Some(s), Some(r))
        } else {
            (None, None)
        };

        // Count selected partitions for progress redraw heuristic
        let selected_count: usize = manifest
            .partitions
            .iter()
            .filter(|u| {
                self.cmd.partitions.is_empty() || self.cmd.partitions.contains(&u.partition_name)
            })
            .count();

        // Strict mode sanity: ensure hashes exist when required
        if self.cmd.strict {
            for update in &manifest.partitions {
                if self.cmd.partitions.is_empty()
                    || self.cmd.partitions.contains(&update.partition_name)
                {
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
        let (partition_dir, created_new_dir) = self.create_partition_dir(metadata.as_ref())?;

        let cleanup_state = Arc::new(Mutex::new(CleanupState {
            files: Vec::new(),
            dir: partition_dir.to_path_buf(),
            dir_is_new: created_new_dir,
        }));

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
                let files = &state.files;
                let dir = &state.dir;
                let dir_is_new = state.dir_is_new;

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

                if dir_is_new && dir.exists() {
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
        std::panic::set_hook(Box::new(move |panic_info| {
            // extract the actual panic message to tell the user what went wrong
            let msg = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
                *s
            } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
                s.as_str()
            } else {
                "Unknown panic"
            };
            let location = panic_info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            eprintln!("\n FATAL THREAD PANIC at {}: {}", location, msg);
            if let Ok(state) = cleanup_state_clone.lock() {
                // Try to remove created files
                for f in &state.files {
                    let _ = fs::remove_file(f);
                }
                // If we created the directory, try to remove it as well
                if state.dir_is_new {
                    let _ = fs::remove_dir_all(&state.dir);
                }
                eprintln!(
                    "Extraction aborted cleanly after a fatal error. Any partially extracted partition images have been deleted to prevent misuse."
                );
            }
        }));

        if !self.cmd.quiet {
            // Inform the user about effective concurrency when -t/--threads is provided
            if let Some(t) = self.cmd.threads
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
        }
        threadpool.scope(|scope| -> Result<()> {
            let multiprogress = MultiProgress::new();

            // create all bars upfront so the network bar stays at the bottom
            let mut pending_partitions = Vec::new();
            for (hash_index_counter, update) in manifest
                .partitions
                .iter()
                .filter(|u| {
                    self.cmd.partitions.is_empty()
                        || self.cmd.partitions.contains(&u.partition_name)
                })
                .enumerate()
            {
                let pb = multiprogress.add(self.create_progress_bar(update)?);
                pending_partitions.push((hash_index_counter, update, pb));
            }

            // append the global network monitor bar below the partition bars
            #[cfg(feature = "remote")]
            {
                if payload_path_str.starts_with("http://")
                    || payload_path_str.starts_with("https://")
                {
                    let mut total_down_size = 0u64;
                    for (_, update, _) in &pending_partitions {
                        for op in &update.operations {
                            total_down_size += op.data_length.unwrap_or(0);
                        }
                    }

                    let bar_style = ProgressStyle::with_template(
                        "\n{prefix:.cyan.bold} {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta})",
                    )
                    .unwrap();

                    let bar_pb =
                        multiprogress.add(ProgressBar::new(total_down_size).with_style(bar_style));
                    bar_pb.set_prefix("Network:");
                    bar_pb.enable_steady_tick(std::time::Duration::from_millis(200));
                    let ok = crate::remote::GLOBAL_NETWORK_STATUS.set(bar_pb).is_ok();
                    debug_assert!(ok, "GLOBAL_NETWORK_STATUS set more than once");
                }
            }

            for (hash_index_counter, update, pb) in pending_partitions {
                if self.process_partition(
                    scope,
                    pb,
                    hash_index_counter,
                    update,
                    payload,
                    block_size,
                    &partition_dir,
                    &cleanup_state,
                    &cancellation_token,
                    &first_error,
                    &stats_sender,
                    &hash_sender,
                    simd,
                    metadata.as_ref(),
                )? {
                    break;
                }
            }
            Ok(())
        })?;

        // clear the network bar once all workers finish
        #[cfg(feature = "remote")]
        {
            if let Some(pb) = crate::remote::GLOBAL_NETWORK_STATUS.get() {
                pb.finish_and_clear();
            }
        }

        // 5. Finalize: Error check, Reporting, and UI
        if cancellation_token.load(Ordering::Acquire) {
            self.handle_critical_error(&cleanup_state, &first_error)?;
        }

        self.print_final_reports(hash_receiver, stats_receiver, total_start, &partition_dir)?;

        if let Ok(mut state) = cleanup_state.lock() {
            state.files.clear();
        }

        Ok(())
    }

    /// Handles a single partition's extraction lifecycle.
    /// Returns Ok(true) if extraction was cancelled.
    #[allow(clippy::too_many_arguments)]
    fn process_partition<'s>(
        &'s self,
        scope: &rayon::Scope<'s>,
        progress_bar: ProgressBar,
        part_index: usize,
        update: &'s PartitionUpdate,
        payload: &'s Payload,
        block_size: usize,
        partition_dir: &Path,
        cleanup_state: &Arc<Mutex<CleanupState>>,
        cancellation_token: &Arc<AtomicBool>,
        first_error: &Arc<Mutex<Option<anyhow::Error>>>,
        stats_sender: &Option<crossbeam_channel::Sender<Stat>>,
        hash_sender: &Option<crossbeam_channel::Sender<HashRec>>,
        simd: CpuSimd,
        metadata: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<bool> {
        self.validate_non_overlapping_extents(&update.operations)
            .with_context(|| format!("Invalid extents in partition '{}'", update.partition_name))?;

        if cancellation_token.load(Ordering::Acquire) {
            eprintln!(
                "Extraction cancelled before processing '{}'",
                update.partition_name
            );
            return Ok(true);
        }

        let (partition_file, partition_len, out_path) =
            self.open_partition_file(update, partition_dir, metadata)?;

        match cleanup_state.lock() {
            Ok(mut guard) => guard.files.push(out_path),
            Err(poisoned) => {
                eprintln!("warning: cleanup lock poisoned by previous panic");
                poisoned.into_inner().files.push(out_path);
            }
        }

        let part_start = if self.cmd.stats {
            Some(Instant::now())
        } else {
            None
        };

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

        let ops = &update.operations;
        let base_ptr = PartitionPtr(partition_file.as_ptr() as *mut u8);

        if ops.len() <= 2 {
            self.dispatch_serial(
                ctx,
                ops,
                payload,
                base_ptr,
                block_size,
                simd,
                progress_bar,
                update,
                part_index,
                part_start,
            )?;
        } else {
            self.dispatch_parallel(
                scope,
                ctx,
                ops,
                payload,
                base_ptr,
                block_size,
                simd,
                progress_bar,
                update,
                part_index,
                part_start,
            );
        }

        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_serial(
        &self,
        ctx: Arc<WorkerContext>,
        ops: &[InstallOperation],
        payload: &Payload,
        base_ptr: PartitionPtr,
        block_size: usize,
        simd: CpuSimd,
        progress_bar: ProgressBar,
        update: &PartitionUpdate,
        part_index: usize,
        part_start: Option<Instant>,
    ) -> Result<()> {
        for op in ops {
            if ctx.cancellation_token.load(Ordering::Acquire) {
                break;
            }

            match self.run_op_raw(
                op,
                payload,
                base_ptr,
                ctx.partition_len,
                block_size,
                &ctx.part_name,
                simd,
                &progress_bar,
            ) {
                Ok(()) => {}
                Err(e) if let Ok(mut slot) = ctx.first_error.lock() => {
                    ctx.cancellation_token.store(true, Ordering::Release);
                    if slot.is_none() {
                        *slot = Some(e.context(format!("Error in partition '{}'", ctx.part_name)));
                    }
                    return Ok(());
                }
                Err(_) => {
                    eprintln!(
                        "\nCritical error: An extraction error occurred, and the system additionally failed to acquire a lock to record the details. Aborting..."
                    );
                    ctx.cancellation_token.store(true, Ordering::Release);
                    return Ok(());
                }
            }
        }

        if !ctx.cancellation_token.load(Ordering::Acquire) {
            self.post_process_partition(&ctx, update, simd, part_index, part_start);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_parallel<'s>(
        &'s self,
        scope: &rayon::Scope<'s>,
        ctx: Arc<WorkerContext>,
        ops: &'s [InstallOperation],
        payload: &'s Payload,
        base_ptr: PartitionPtr,
        block_size: usize,
        simd: CpuSimd,
        progress_bar: ProgressBar,
        update: &'s PartitionUpdate,
        part_index: usize,
        part_start: Option<Instant>,
    ) {
        let chunk_size = if ops.len() < 64 { 8 } else { 16 };

        for chunk in ops.chunks(chunk_size) {
            let pb = progress_bar.clone();
            let ctx = ctx.clone();

            scope.spawn(move |_| {
                for op in chunk {
                    if ctx.cancellation_token.load(Ordering::Acquire) { return; }

                    match self.run_op_raw(op, payload, base_ptr, ctx.partition_len, block_size, &ctx.part_name, simd, &pb) {
                        Ok(()) => {},
                        Err(e) if let Ok(mut slot) = ctx.first_error.lock() => {
                            ctx.cancellation_token.store(true, Ordering::Release);
                            if slot.is_none() {
                                *slot = Some(e.context(format!("Error in partition '{}'", ctx.part_name)));
                            }
                            return;
                        }
                        Err(_) => {
                            eprintln!("\nCritical error: An extraction error occurred, and the system additionally failed to acquire a lock to record the details. Aborting...");
                            ctx.cancellation_token.store(true, Ordering::Release);
                            return;
                        }
                    }
                }

                if ctx.remaining_ops.fetch_sub(chunk.len(), Ordering::AcqRel) == chunk.len() {
                    self.post_process_partition(&ctx, update, simd, part_index, part_start);
                }
            });
        }
    }

    fn handle_critical_error(
        &self,
        cleanup_state: &Arc<Mutex<CleanupState>>,
        first_error: &Arc<Mutex<Option<anyhow::Error>>>,
    ) -> Result<()> {
        if let Ok(state) = cleanup_state.lock() {
            for f in &state.files {
                let _ = fs::remove_file(f);
            }
            if state.dir_is_new {
                let _ = fs::remove_dir_all(&state.dir);
            }
        }
        let error = match first_error.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => {
                eprintln!("warning: error lock poisoned by previous panic");
                poisoned.into_inner().take()
            }
        };
        if let Some(err) = error {
            eprintln!("\n{:?}", err);
        }
        bail!(
            "❌ Extraction failed due to errors (see above). All partial files have been cleaned up."
        );
    }

    fn print_final_reports(
        &self,
        hash_receiver: Option<crossbeam_channel::Receiver<HashRec>>,
        stats_receiver: Option<crossbeam_channel::Receiver<Stat>>,
        total_start: Option<Instant>,
        partition_dir: &Path,
    ) -> Result<()> {
        if let Some(receiver) = hash_receiver {
            let mut v: Vec<HashRec> = Vec::new();
            while let Ok(r) = receiver.try_recv() {
                v.push(r);
            }
            if !v.is_empty() {
                v.sort_by_key(|r| r.order);
                println!("Partition hashes (SHA-256):");
                for r in v {
                    println!("{}: sha256={}", r.name, r.hex);
                }
            }
        }

        if let Some(receiver) = stats_receiver {
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

        if !self.cmd.quiet {
            self.display_extracted_folder_size(partition_dir)?;
        }

        if !self.cmd.no_open && !self.cmd.quiet {
            self.open_extracted_folder(partition_dir)?;
        }

        Ok(())
    }

    fn create_progress_bar(&self, update: &PartitionUpdate) -> Result<ProgressBar> {
        if self.cmd.quiet {
            return Ok(ProgressBar::hidden());
        }

        let total_bytes = update
            .new_partition_info
            .as_ref()
            .and_then(|i| i.size)
            .unwrap_or(0);

        let style = ProgressStyle::with_template(
            "{prefix:>24!.green.bold} [{wide_bar:.white.dim}] {percent:>3}%",
        )
        .context("unable to build progress bar template")?
        .progress_chars("=> ");

        Ok(ProgressBar::new(total_bytes)
            .with_finish(ProgressFinish::Abandon) // FREEZE the bar exactly where it crashed
            .with_prefix(update.partition_name.to_string())
            .with_style(style))
    }

    fn post_process_partition(
        &self,
        ctx: &WorkerContext,
        update: &PartitionUpdate,
        simd: CpuSimd,
        part_index: usize,
        part_start: Option<Instant>,
    ) {
        let is_cancelled = || ctx.cancellation_token.load(Ordering::Acquire);

        let final_slice: &[u8] = &ctx.partition_file;

        let mut computed_digest_opt: Option<[u8; 32]> = None;

        if !self.cmd.no_verify {
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
                        return;
                    }
                }
            } else if self.cmd.strict {
                ctx.cancellation_token.store(true, Ordering::Release);
                eprintln!(
                    "\nCritical error: Strict mode: missing partition hash for '{}'",
                    ctx.part_name
                );
                return;
            }
        }

        if is_cancelled() {
            return;
        }

        if self.cmd.sanity && is_all_zero_with_simd(simd, final_slice) {
            ctx.cancellation_token.store(true, Ordering::Release);
            eprintln!(
                "\nCritical error: Sanity check failed for '{}'",
                ctx.part_name
            );
            return;
        }

        if is_cancelled() {
            return;
        }

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
            let _ = sender.send(HashRec {
                order: part_index,
                name: ctx.part_name.to_string(),
                hex: hexstr,
            });
        }

        if let (Some(start), Some(sender)) = (part_start, ctx.stats_sender.as_ref()) {
            let elapsed = start.elapsed();
            let _ = sender.send(Stat {
                name: ctx.part_name.to_string(),
                bytes: ctx.partition_len as u64,
                ms: elapsed.as_millis(),
            });
        }
    }

    /// # Soundness
    /// This function is the core of otaripper's high-performance extraction.
    /// It is sound because:
    /// 1.`base_ptr` is guaranteed to be valid for `partition_len` as long as the
    ///   `partition_file` Mmap is alive.
    /// 2. Scoped threads (`rayon::scope`) ensure that worker threads cannot outlive
    ///   the `Mmap` lifetime.
    /// 3. `validate_non_overlapping_extents` proves that no two threads can receive
    ///   the same memory range, preventing data races and mutable aliasing UB.
    #[allow(clippy::too_many_arguments)]
    fn run_op_raw(
        &self,
        op: &InstallOperation,
        payload: &Payload,
        base_ptr: PartitionPtr,
        partition_len: usize,
        block_size: usize,
        partition_name: &str,
        simd: CpuSimd,
        pb: &ProgressBar,
    ) -> Result<()> {
        let op_type = Type::try_from(op.r#type)?;
        let raw_extents =
            self.extract_dst_extents_raw(op, base_ptr.0, partition_len, block_size)?;

        // SAFETY: Reconstitute pointer inside the thread.
        // Sound because extents are non-overlapping and threads are scoped to the Mmap lifetime.
        let mut dst_extents = Vec::with_capacity(raw_extents.len());

        for (ptr, len) in raw_extents {
            dst_extents.push(unsafe { slice::from_raw_parts_mut(ptr, len) });
        }

        let total_dst_size: usize = dst_extents.iter().map(|e| e.len()).sum();

        match op_type {
            Type::Replace => {
                let data = self.extract_data(op, payload, pb, total_dst_size)?;
                self.run_op_replace_slice(
                    &*data,
                    &mut dst_extents,
                    block_size,
                    total_dst_size,
                    simd,
                )?;
                if matches!(payload.data, crate::payload::PayloadData::Local(_)) {
                    pb.inc(total_dst_size as u64);
                }
                Ok(())
            }
            Type::ReplaceBz => {
                let data = self.extract_data(op, payload, pb, total_dst_size)?;
                let mut decoder = BzDecoder::new(&*data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size, simd)?;
                if matches!(payload.data, crate::payload::PayloadData::Local(_)) {
                    pb.inc(total_dst_size as u64);
                }
                Ok(())
            }
            Type::ReplaceXz => {
                let data = self.extract_data(op, payload, pb, total_dst_size)?;
                let mut decoder = liblzma::read::XzDecoder::new(&*data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size, simd)?;
                if matches!(payload.data, crate::payload::PayloadData::Local(_)) {
                    pb.inc(total_dst_size as u64);
                }
                Ok(())
            }
            Type::Zero | Type::Discard => {
                // the OS kernel guarantees new file sectors are strictly zeroed.
                // we safely bypass user-space filling completely to save disk I/O
                pb.inc(total_dst_size as u64);
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

        // FAST PATH: Single extent zero-copy decompressive read directly into memory-mapped file
        if dst_extents.len() == 1 {
            let dst = &mut dst_extents[0];
            let mut total_read = 0;
            loop {
                match reader.read(&mut dst[total_read..]) {
                    Ok(0) => break,
                    Ok(n) => total_read += n,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        return Err(e)
                            .context("failed to read decompressed data directly to extent");
                    }
                }
            }
            let bytes_read_aligned = total_read.div_ceil(block_size) * block_size;
            ensure!(
                bytes_read_aligned == dst_len,
                "more dst blocks than data, even with padding"
            );

            // Force the decoder to hit EOF to trigger trailing CRC/checksum logic, properly bubbling up any I/O errors
            let mut eof_check = [0u8; 1];
            let extra_bytes = reader.read(&mut eof_check).context("failed to check EOF")?;
            ensure!(
                extra_bytes == 0,
                "stream contained more data than extent capacity"
            );
            return Ok(());
        }

        // BUFFERED PATH: Multi-extent using thread local 1MB buffer for optimal SIMD triggers
        let mut total_read = 0usize;
        let mut writer = ExtentsWriter::new(dst_extents, simd);

        COPY_BUFFER.with(|buf_cell| {
            let mut buf = buf_cell.borrow_mut();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        writer
                            .write_all(&buf[..n])
                            .context("failed to write to ExtentsWriter")?;
                        total_read += n;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e).context("failed to read from decompressor"),
                }
            }
            Ok::<(), anyhow::Error>(())
        })?;

        let bytes_read_aligned = total_read.div_ceil(block_size) * block_size;
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

        let bytes_read_aligned = bytes_read.div_ceil(block_size) * block_size;

        ensure!(
            bytes_read_aligned == total_dst_size,
            "more dst blocks than data, even with padding"
        );

        // FAST PATH: single contiguous extent
        if dst_extents.len() == 1 {
            let dst = &mut dst_extents[0];
            let target = &mut dst[..bytes_read];

            // Large write-once buffers: avoid cache pollution
            if bytes_read >= 1024 * 1024 {
                simd_copy_large(simd, data, target);
            } else {
                target.copy_from_slice(data);
            }

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

    fn open_payload_file(&self, path_str: &str) -> Result<PayloadSource> {
        use sysinfo::System;
        use tempfile::NamedTempFile;

        // HTTP remote reader logic here
        if path_str.starts_with("http://") || path_str.starts_with("https://") {
            #[cfg(feature = "remote")]
            {
                // safely detect if we are on Android or not
                // we will check for build.prop (Android) and resolv.conf (Linux)
                let is_android = std::path::Path::new("/system/build.prop").exists();
                let has_resolv_conf = std::path::Path::new("/etc/resolv.conf").exists();

                // compile time detect Windows (which lacks resolv.conf but should use Hickory)
                let is_windows = cfg!(target_os = "windows");

                // Only use Hickory if we are NOT on Android, and we either have resolv.conf or we are on Windows
                let use_hickory = !is_android && (has_resolv_conf || is_windows);

                let client = reqwest::blocking::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .pool_max_idle_per_host(32)
                    .tcp_nodelay(true)
                    .tcp_keepalive(std::time::Duration::from_secs(5))
                    .user_agent(concat!("otaripper/", env!("CARGO_PKG_VERSION")))
                    .hickory_dns(use_hickory)
                    .build()
                    .context("Failed to build HTTP client")?;

                // Initialize a spinner for the initial setup phase
                let spinner = indicatif::ProgressBar::new_spinner();
                spinner.set_style(
                    indicatif::ProgressStyle::with_template("{spinner:.cyan.bold} {msg}").unwrap(),
                );
                spinner.enable_steady_tick(std::time::Duration::from_millis(69));

                let mut reader =
                    crate::remote::CachingHttpReader::new(client.clone(), path_str, &spinner)?;

                // revert to a simple spinner for parsing (exact bytes are unknown here)
                spinner.set_style(
                    indicatif::ProgressStyle::with_template("{spinner:.cyan.bold} {msg}").unwrap(),
                );
                spinner.set_message("Parsing remote payload metadata...");

                let mut magic = [0u8; 4];
                reader
                    .read_exact(&mut magic)
                    .context("Failed to read remote header")?;
                reader.seek(std::io::SeekFrom::Start(0))?;

                let mut base_offset = 0;
                if &magic == b"PK\x03\x04" {
                    spinner.set_message("Locating payload.bin inside remote ZIP...");
                    let mut archive = ZipArchive::new(&mut reader)?;
                    let zipfile = archive
                        .by_name("payload.bin")
                        .context("Your ZIP URL does not contain payload.bin")?;
                    base_offset = zipfile
                        .data_start()
                        .context("Failed to find data_start inside ZIP URL")?;
                }

                spinner.set_message("Reading payload headers...");
                reader.seek(std::io::SeekFrom::Start(base_offset))?;
                let mut header = [0u8; 24];
                reader.read_exact(&mut header)?;

                let file_format_version = u64::from_be_bytes(header[4..12].try_into().unwrap());
                let manifest_size = u64::from_be_bytes(header[12..20].try_into().unwrap());

                // prevent exabyte OOM panics from malicious servers sending garbage data
                // max manifest size is 256MB, anything larger is mathematically a rogue server
                anyhow::ensure!(
                    manifest_size <= 256 * 1024 * 1024,
                    "Server sent impossible manifest size ({} bytes), The connection is corrupt or malicious!",
                    manifest_size
                );

                let (header_size, sig_size) = if file_format_version >= 2 {
                    (
                        24,
                        u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize,
                    )
                } else {
                    (20, 0)
                };

                let total_metadata_size = header_size + manifest_size as usize + sig_size;

                // change the spinner into a real little progress bar for the manifest download!
                spinner.set_length(total_metadata_size as u64);
                spinner.set_style(
                    indicatif::ProgressStyle::with_template("{spinner:.cyan.bold} {msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})")
                        .unwrap()
                        .progress_chars("=> ")
                );
                spinner.set_message("Downloading Android manifest...");

                // fetch the manifest in one fast, persistent connection
                let manifest_buf = crate::remote::fetch_http_chunk(
                    &client,
                    path_str,
                    base_offset,
                    total_metadata_size,
                    &spinner,
                    total_metadata_size,
                )?;

                // vanish the initialization spinner entirely so the normal extraction bars can cleanly show up
                spinner.finish_and_clear();

                return Ok(PayloadSource::Remote {
                    manifest_buf,
                    url: path_str.to_string(),
                    payload_base_offset: base_offset,
                    client,
                });
            }

            #[cfg(not(feature = "remote"))]
            {
                anyhow::bail!(
                    "HTTP streaming is disabled! as this is a Lite build to save binary space\n\
                    To extract directly from URL, please use the full version of otaripper."
                );
            }
        }

        let path = Path::new(path_str);
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
                if zipfile.compression() == zip::CompressionMethod::Stored {
                    let base_offset = zipfile.data_start().ok_or_else(|| anyhow::anyhow!("Failed to determine the starting byte offset of payload.bin inside the ZIP"))?;
                    let mmap = unsafe { Mmap::map(&file) }.context("failed to mmap zip file")?;
                    return Ok(PayloadSource::MappedOffset(mmap, base_offset as usize));
                }

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
                    let temp_file = if let Some(ref out_dir) = self.cmd.output_dir {
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
        _metadata: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<(Arc<MmapMut>, usize, PathBuf)> {
        let partition_len = update
            .new_partition_info
            .as_ref()
            .and_then(|info| info.size)
            .context("unable to determine output file size")?;

        let filename = Path::new(&update.partition_name).with_extension("img");
        let path: PathBuf = if self.cmd.fff {
            let subfolder = match update.partition_name.as_str() {
                "boot" | "init_boot" | "recovery" | "vbmeta" | "vbmeta_system" | "vbmeta_vendor" | "vendor_boot" => "BOOTLOADER",
                "abl" | "aop" | "aop_config" | "bluetooth" | "cpucp" | "cpucp_dtb" | "devcfg" | "dsp" | "dtbo" | "engineering_cdt" | "featenabler" | "hyp" | "imagefv" | "keymaster" | "oplus_sec" | "oplusstanvbk" | "qupfw" | "shrm" | "splash" | "tz" | "uefi" | "uefisecapp" | "xbl" | "xbl_config" | "xbl_ramdump" => "CRITICAL",
                "modem" => "MODEM",
                "my_bigball" | "my_carrier" | "my_engineering" | "my_heytap" | "my_manifest" | "my_product" | "my_region" | "my_stock" | "odm" | "product" | "system" | "system_dlkm" | "system_ext" | "vendor" | "vendor_dlkm" => "SYSTEM",
                _ => "EXTRA",
            };

            let target_dir = partition_dir.as_ref().join(subfolder);
            fs::create_dir_all(&target_dir)
                .with_context(|| format!("could not create subfolder directory: {target_dir:?}"))?;
            target_dir.join(filename)
        } else {
            partition_dir.as_ref().join(filename)
        };

        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mmap = {
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create_new(true);

            // windows-only optimization: aggressive kernel readahead
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::fs::OpenOptionsExt;
                opts.custom_flags(0x08000000); // FILE_FLAG_SEQUENTIAL_SCAN
            }

            let file = opts
                .open(&path)
                .with_context(|| format!("unable to open file for writing: {path:?}"))?;

            // Linux-only optimization: Pre-allocate physical disk blocks
            // this prevents sparse file fragmentation and eliminates filesystem lock
            // contention when dozens of threads are writing randomly.
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::io::AsRawFd;
                unsafe {
                    let fd = file.as_raw_fd();
                    let ret = libc::posix_fallocate(fd, 0, partition_len as libc::off_t);
                    if ret != 0 {
                        eprintln!(
                            "Warning: posix_fallocate failed (code {}), Extraction continues but may be fragmented",
                            ret
                        );
                    }
                }
            }

            file.set_len(partition_len)?;

            let mmap_mut = unsafe { MmapMut::map_mut(&file) }
                .with_context(|| format!("failed to mmap file: {path:?}"))?;

            // Linux-only sequential access hint (clean memmap2 API)
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = mmap_mut.advise(memmap2::Advice::Sequential) {
                    // We use eprintln for a simple warning that doesn't stop extraction
                    eprintln!("Warning: Failed to set sequential readahead hint: {}", e);
                }
            }
            mmap_mut
        };

        let partition = Arc::new(mmap);
        Ok((partition, partition_len as usize, path))
    }

    #[allow(unused_variables)]
    fn extract_data<'b>(
        &self,
        op: &InstallOperation,
        payload: &'b Payload,
        pb: &ProgressBar,
        total_dst_size: usize,
    ) -> Result<std::borrow::Cow<'b, [u8]>> {
        let data_len = op.data_length.context("data_length not defined")? as usize;
        let offset = op.data_offset.context("data_offset not defined")? as usize;

        let data = match &payload.data {
            crate::payload::PayloadData::Local(local_slice) => {
                let end_offset = offset
                    .checked_add(data_len)
                    .context("data_offset + data_length overflows")?;
                ensure!(
                    end_offset <= local_slice.len(),
                    "data range {}..{} exceeds payload size {}",
                    offset,
                    end_offset,
                    local_slice.len()
                );
                std::borrow::Cow::Borrowed(&local_slice[offset..end_offset])
            }
            #[cfg(feature = "remote")]
            crate::payload::PayloadData::Remote {
                url,
                data_offset,
                client,
            } => {
                let start = data_offset + offset as u64;
                let buf = crate::remote::fetch_http_chunk(
                    client,
                    url,
                    start,
                    data_len,
                    pb,
                    total_dst_size,
                )?;
                std::borrow::Cow::Owned(buf)
            }
        };

        if !self.cmd.no_verify {
            if let Some(hash) = &op.data_sha256_hash {
                self.verify_sha256(&*data, hash).context("hash mismatch")?;
            }
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
    /// Implementation uses an O(n log n) sorted interval sweep.
    /// This is acceptable because extents per partition are typically small.
    fn validate_non_overlapping_extents(&self, operations: &[InstallOperation]) -> Result<()> {
        // Heuristic: Most operations have 1-2 extents. Pre-allocating 20% more than ops
        // minimizes reallocations without wasting significant memory.
        let mut extents: Vec<(u64, u64)> =
            Vec::with_capacity(operations.len() + (operations.len() / 5));

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

        for w in extents.windows(2) {
            let prev = w[0];
            let curr = w[1];

            ensure!(
                curr.0 >= prev.1,
                "Overlapping destination extents detected: {} < {}",
                curr.0,
                prev.1
            );
        }

        Ok(())
    }

    fn create_partition_dir(
        &self,
        metadata: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<(PathBuf, bool)> {
        let now = Local::now();
        let os_version = metadata.and_then(|m| m.get("version_name").map(|v| v.as_str()));
        let os_version_safe =
            os_version.map(|v| v.replace(|c| c == '/' || c == '\\' || c == ' ' || c == ':', "_"));

        let folder_name = match os_version_safe {
            Some(v) => {
                if self.cmd.fff {
                    v
                } else {
                    format!("extracted_{}_{}", v, now.format("%Y-%m-%d_%H-%M-%S"))
                }
            }
            None => {
                if self.cmd.fff {
                    "firmware".to_string()
                } else {
                    format!("extracted_{}", now.format("%Y-%m-%d_%H-%M-%S"))
                }
            }
        };

        let dir = match &self.cmd.output_dir {
            Some(output_base) => output_base.join(&folder_name),
            None => {
                let current_dir = env::current_dir().with_context(|| {
                    "Failed to determine current directory. Please specify --output-dir explicitly."
                })?;
                current_dir.join(&folder_name)
            }
        };
        let existed = dir.exists();
        fs::create_dir_all(&dir)
            .with_context(|| format!("could not create output directory: {dir:?}"))?;

        if self.cmd.fff {
            let extra_dir = dir.join("EXTRA");
            fs::create_dir_all(&extra_dir)
                .with_context(|| format!("could not create EXTRA directory: {extra_dir:?}"))?;
        }

        Ok((dir, !existed))
    }

    fn get_threadpool(&self) -> Result<ThreadPool> {
        let mut builder = ThreadPoolBuilder::new();
        if let Some(t) = self.cmd.threads
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

        // print Network Bytes if anywere downloaded
        #[cfg(feature = "remote")]
        {
            let net_bytes =
                crate::remote::NETWORK_BYTES_READ.load(std::sync::atomic::Ordering::Relaxed);
            if net_bytes > 0 {
                let bold_cyan = Style::new().bold().cyan();
                println!(
                    "Total downloaded size: {}",
                    bold_cyan.apply_to(indicatif::HumanBytes(net_bytes as u64))
                );
            }
        }

        let bold_bright_blue = Style::new().bold().blue();
        println!(
            "Tool Source: {}",
            bold_bright_blue.apply_to("https://github.com/syedinsaf/otaripper")
        );
        Ok(())
    }

    /// Recursively calculate the size of a directory and its contents
    fn calculate_directory_size(&self, path: &Path) -> Result<u64> {
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read metadata for: {}", path.display()));
            }
        };

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
        cfg_select! {
            target_os = "windows" => {
                use std::process::{Command, Stdio};
                let _ = Command::new("explorer")
                    .arg(dir_path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| eprintln!("Warning: Failed to open folder: {}", e));
            }
            target_os = "macos" => {
                use std::process::{Command, Stdio};
                let _ = Command::new("open")
                    .arg(dir_path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| eprintln!("Warning: Failed to open folder: {}", e));
            }
            target_os = "linux" => {
                use std::process::{Command, Stdio};

                // On KDE, using xdg-open triggers a harmless but noisy Qt portal warning (related to kioclient registration).
                // Spawning Dolphin directly avoids this, while still opening the folder correctly.
                if Command::new("dolphin").arg(dir_path).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
                    return Ok(());
                }

                // Fallback to standard xdg-open for non-KDE desktops
                if Command::new("xdg-open").arg(dir_path).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
                    return Ok(());
                }

                eprintln!("Warning: Unable to open folder (dolphin and xdg-open failed)");
            }
            _ => {}
        }

        Ok(())
    }
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

    fn reorganize_extracted_dir(&self, dir: &Path) -> Result<()> {
        let dir_name = dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("extracted");

        let device_name = if dir_name.starts_with("extracted_") {
            let without_prefix = &dir_name["extracted_".len()..];
            if without_prefix.len() > 20 {
                let suffix_start = without_prefix.len() - 20;
                let potential_suffix = &without_prefix[suffix_start..];
                let chars: Vec<char> = potential_suffix.chars().collect();
                if chars[0] == '_' && chars[5] == '-' && chars[8] == '-' && chars[11] == '_' && chars[14] == '-' && chars[17] == '-' {
                    without_prefix[..suffix_start].to_string()
                } else {
                    without_prefix.to_string()
                }
            } else {
                without_prefix.to_string()
            }
        } else {
            dir_name.to_string()
        };

        let parent_dir = dir.parent().unwrap_or_else(|| Path::new("."));
        let target_base_dir = parent_dir.join(&device_name);

        println!("Reorganizing extracted directory: {}", dir.display());
        println!("Target Directory: {}", target_base_dir.display());

        // Pre-create EXTRA folder when reorganizing
        let extra_dir = target_base_dir.join("EXTRA");
        fs::create_dir_all(&extra_dir)
            .with_context(|| format!("could not create EXTRA directory: {extra_dir:?}"))?;

        let mut moved_count = 0;

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("img") {
                let file_name = path.file_name().unwrap().to_string_lossy();
                let stem = path.file_stem().unwrap().to_string_lossy();

                let subfolder = match stem.as_ref() {
                    "boot" | "init_boot" | "recovery" | "vbmeta" | "vbmeta_system" | "vbmeta_vendor" | "vendor_boot" => "BOOTLOADER",
                    "abl" | "aop" | "aop_config" | "bluetooth" | "cpucp" | "cpucp_dtb" | "devcfg" | "dsp" | "dtbo" | "engineering_cdt" | "featenabler" | "hyp" | "imagefv" | "keymaster" | "oplus_sec" | "oplusstanvbk" | "qupfw" | "shrm" | "splash" | "tz" | "uefi" | "uefisecapp" | "xbl" | "xbl_config" | "xbl_ramdump" => "CRITICAL",
                    "modem" => "MODEM",
                    "my_bigball" | "my_carrier" | "my_engineering" | "my_heytap" | "my_manifest" | "my_product" | "my_region" | "my_stock" | "odm" | "product" | "system" | "system_dlkm" | "system_ext" | "vendor" | "vendor_dlkm" => "SYSTEM",
                    _ => "EXTRA",
                };

                let target_dir = target_base_dir.join(subfolder);
                fs::create_dir_all(&target_dir)?;
                let target_path = target_dir.join(&*file_name);
                fs::copy(&path, &target_path)?;
                println!("  Copied: {} -> {}/{}", file_name, device_name, subfolder);
                moved_count += 1;
            }
        }

        if moved_count > 0 {
            println!("\nSuccessfully reorganized {} partition images!", moved_count);
            if ask_yes_no(&format!("\nDelete original directory? [y/N]: ")) {
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    println!("  ⚠️ Failed to delete original directory: {}", e);
                } else {
                    println!("  Removed original directory: {}", dir.display());
                }
            }
        } else {
            println!("\nNo .img files found at the root of the directory to reorganize.");
        }

        Ok(())
    }
}

fn ask_yes_no(prompt: &str) -> bool {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}
