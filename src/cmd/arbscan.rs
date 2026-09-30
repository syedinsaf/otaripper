use std::fmt;
use std::fs::{File, write};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::cmd::Cmd;
use crate::cmd::extractor::Extractor;
use serde::Serialize;
use zip::ZipArchive;

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

fn find_files_in_dir(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            find_files_in_dir(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

const HASH_HDR_SIZE: usize = 36;
const HASH_SCAN_MAX: usize = 0x1000;
const MAX_SEGMENT_SIZE: u64 = 20 * 1024 * 1024; // 20 MB safety cap

#[derive(Serialize)]
struct ArbMetadata {
    device_model: String,
    update_label: String,

    image: String,
    major: u32,
    minor: u32,
    arb: u32,
    hash_offset: u64,
    hash_size: u64,
}

#[derive(Debug)]
enum ArbError {
    Io(io::Error),
    InvalidElf(&'static str),
    MissingMetadata(&'static str),
    Serde(serde_json::Error),
}

impl fmt::Display for ArbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArbError::Io(e) => write!(f, "I/O error: {}", e),
            ArbError::InvalidElf(msg) => write!(f, "Invalid ELF: {}", msg),
            ArbError::MissingMetadata(msg) => write!(f, "Metadata error: {}", msg),
            ArbError::Serde(e) => write!(f, "JSON error: {}", e),
        }
    }
}

impl std::error::Error for ArbError {}

impl From<io::Error> for ArbError {
    fn from(e: io::Error) -> Self {
        ArbError::Io(e)
    }
}

impl From<serde_json::Error> for ArbError {
    fn from(e: serde_json::Error) -> Self {
        ArbError::Serde(e)
    }
}

// helpers
fn read_le16(buf: &[u8], off: usize) -> Option<u16> {
    buf.get(off..)?
        .first_chunk()
        .map(|b| u16::from_le_bytes(*b))
}

fn read_le32(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..)?
        .first_chunk()
        .map(|b| u32::from_le_bytes(*b))
}

fn read_le64(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..)?
        .first_chunk()
        .map(|b| u64::from_le_bytes(*b))
}

fn sane_version(v: u32) -> bool {
    v < 1000
}

// ARB = 0 is VALID (OOS, OnePlus)
fn sane_arb(v: u32) -> bool {
    v < 128
}

fn ask_yes_no(prompt: &str) -> bool {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn ask_string(prompt: &str) -> String {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    input.trim().to_string()
}

fn json_filename(device_model: &str, update_label: &str, arb: u32, input: &Path) -> String {
    let mut stem = String::new();

    // Trim leading/trailing whitespace, slashes, and backslashes
    let dev = device_model.trim_matches(|c| c == '/' || c == '\\' || c == ' ' || c == '\t');
    let upd = update_label.trim_matches(|c| c == '/' || c == '\\' || c == ' ' || c == '\t');

    if !dev.is_empty() || !upd.is_empty() {
        if !dev.is_empty() {
            stem.push_str(dev);
        }
        if !upd.is_empty() {
            if !stem.is_empty() {
                stem.push('_');
            }
            stem.push_str(upd);
        }
    } else {
        // Fall back to the original file/URL stem parsing
        let full_str = input.to_string_lossy();
        let is_url = full_str.starts_with("http://") || full_str.starts_with("https://");

        let base_name = if is_url {
            // Strip query parameters first
            let without_query = match full_str.find('?') {
                Some(pos) => &full_str[..pos],
                None => &full_str,
            };
            // Extract the last component after '/'
            match without_query.rfind('/') {
                Some(pos) => &without_query[pos + 1..],
                None => without_query,
            }
        } else {
            // Otherwise, treat as a local path and use Rust's standard file_stem
            input
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output")
        };

        // Strip extension (like .zip) if present, but only if it's a URL (local file_stem already stripped it)
        let mut extracted_stem = base_name;
        if is_url
            && let Some(s) = Path::new(extracted_stem)
                .file_stem()
                .and_then(|s| s.to_str())
        {
            extracted_stem = s;
        }
        stem.push_str(extracted_stem);
    }

    // Sanitize any remaining characters that are illegal in Windows/Linux filesystems
    let sanitized: String = stem
        .chars()
        .map(|c| match c {
            '/' | '\\' | '?' | '%' | '*' | ':' | '|' | '"' | '<' | '>' | ' ' => '_',
            _ => c,
        })
        .collect();

    let final_stem = if sanitized.is_empty() {
        "output".to_string()
    } else {
        sanitized
    };
    format!("{}_ARB({}).json", final_stem, arb)
}

// HASH header detection
fn find_hash_header(seg: &[u8]) -> Option<usize> {
    for off in (0..HASH_SCAN_MAX.min(seg.len())).step_by(4) {
        if off + HASH_HDR_SIZE > seg.len() {
            break;
        }

        let version = read_le32(seg, off)?;
        let common_sz = read_le32(seg, off + 4)? as usize;
        let qti_sz = read_le32(seg, off + 8)? as usize;
        let oem_sz = read_le32(seg, off + 12)? as usize;
        let hash_tbl_sz = read_le32(seg, off + 16)? as usize;

        if !(1..=10).contains(&version) {
            continue;
        }
        if common_sz > 0x1000 || qti_sz > 0x1000 || oem_sz > 0x4000 {
            continue;
        }
        // Support both SHA256 (multiple of 32) and SHA384 (multiple of 48) hash tables by checking for a multiple of 16
        if hash_tbl_sz == 0 || !hash_tbl_sz.is_multiple_of(16) {
            continue;
        }
        if off + HASH_HDR_SIZE + common_sz + qti_sz + oem_sz > seg.len() {
            continue;
        }

        return Some(off);
    }
    None
}

pub fn run(no_json: bool, path: &Path) -> anyhow::Result<()> {
    let path_str = path.to_str().unwrap_or("");
    let is_url = path_str.starts_with("http://") || path_str.starts_with("https://");

    let metadata = crate::cmd::metadata::fetch_metadata(path_str);

    let mut is_zip = false;
    let mut is_elf = false;

    #[cfg(feature = "remote")]
    let mut remote_reader = None;

    if is_url {
        #[cfg(feature = "remote")]
        {
            println!("[arbscan] Connecting to remote server...");
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .pool_max_idle_per_host(32)
                .tcp_nodelay(true)
                .tcp_keepalive(std::time::Duration::from_secs(5))
                .user_agent(concat!("otaripper/", env!("CARGO_PKG_VERSION")))
                .build()?;

            let spinner = indicatif::ProgressBar::new_spinner();
            spinner.set_style(
                indicatif::ProgressStyle::with_template("{spinner:.cyan.bold} {msg}").unwrap(),
            );
            spinner.enable_steady_tick(std::time::Duration::from_millis(69));

            let mut reader = crate::remote::CachingHttpReader::new(client, path_str, &spinner)?;
            spinner.finish_and_clear();

            let mut magic = [0u8; 4];
            reader.read_exact(&mut magic)?;
            reader.seek(SeekFrom::Start(0))?;

            if &magic == b"PK\x03\x04" {
                is_zip = true;
            } else if magic == [0x7f, b'E', b'L', b'F'] {
                is_elf = true;
            }
            remote_reader = Some(reader);
        }
        #[cfg(not(feature = "remote"))]
        {
            anyhow::bail!("HTTP remote URL streaming is not supported in this build!");
        }
    } else if path.is_dir() {
        println!("[arbscan] Directory detected. Scanning for bootloader image...");
        let mut files = Vec::new();
        find_files_in_dir(path, &mut files)?;

        let mut candidate_path = None;
        let mut best_priority = usize::MAX;

        for p in files {
            if let Some(filename) = p.file_name().and_then(|f| f.to_str()) {
                let name_lower = filename.to_lowercase();
                let priority = if name_lower == "xbl_config.img" {
                    1
                } else if name_lower == "xbl_config.elf" {
                    2
                } else if name_lower == "xbl.img" {
                    3
                } else if name_lower == "xbl.elf" {
                    4
                } else {
                    0
                };

                if priority > 0 && priority < best_priority {
                    best_priority = priority;
                    candidate_path = Some(p);
                }
            }
        }

        let candidate_path = candidate_path.ok_or_else(|| {
            anyhow::anyhow!("No valid bootloader candidate (xbl_config.img/elf, xbl.img/elf) found in the directory!")
        })?;

        println!("[arbscan] Found candidate: {}", candidate_path.display());
        return match do_run(no_json, &candidate_path, path, metadata) {
            Ok(()) => Ok(()),
            Err(e) => anyhow::bail!("{}", e),
        };
    } else {
        // Local file
        let mut magic = [0u8; 4];
        if let Ok(mut f) = File::open(path) {
            let _ = f.read_exact(&mut magic);
        }
        if magic == [0x7f, b'E', b'L', b'F'] {
            is_elf = true;
        } else if &magic == b"PK\x03\x04" {
            is_zip = true;
        }
    }

    if is_elf {
        if is_url {
            #[cfg(feature = "remote")]
            {
                if let Some(mut reader) = remote_reader {
                    println!("[arbscan] Remote ELF image detected. Downloading to scan...");
                    let temp_dir = tempfile::tempdir()?;
                    let temp_file_path = temp_dir.path().join("extracted_elf.img");
                    {
                        let mut temp_file = File::create(&temp_file_path)?;
                        std::io::copy(&mut reader, &mut temp_file)?;
                    }
                    return match do_run(no_json, &temp_file_path, path, metadata) {
                        Ok(()) => Ok(()),
                        Err(e) => anyhow::bail!("{}", e),
                    };
                }
            }
        } else {
            // Direct local ELF image
            return match do_run(no_json, path, path, metadata) {
                Ok(()) => Ok(()),
                Err(e) => anyhow::bail!("{}", e),
            };
        }
    }

    if is_zip {
        // It's a ZIP archive. Let's see what's inside.
        let mut reader: Box<dyn ReadSeek> = if is_url {
            #[cfg(feature = "remote")]
            {
                if let Some(r) = remote_reader {
                    Box::new(r)
                } else {
                    anyhow::bail!("Remote reader was not initialized.");
                }
            }
            #[cfg(not(feature = "remote"))]
            {
                anyhow::bail!("HTTP remote URL streaming is not supported in this build!");
            }
        } else {
            let file = File::open(path)?;
            Box::new(file)
        };

        let mut archive = ZipArchive::new(&mut reader)?;

        // Check if it is a standard OTA payload zip by checking for payload.bin
        let is_ota = archive.by_name("payload.bin").is_ok();

        if is_ota {
            println!("[arbscan] OTA package detected. Extracting xbl_config.img temporarily...");
            let temp_dir = tempfile::tempdir()?;
            let cmd = Cmd {
                subcmd: None,
                list: false,
                threads: None,
                output_dir: Some(temp_dir.path().to_path_buf()),
                partitions: vec!["xbl_config".to_string()],
                no_verify: true,
                strict: false,
                print_hash: false,
                sanity: false,
                stats: false,
                no_open: true,
                positional_payload: Some(path.to_path_buf()),
                quiet: true,
            };

            let extractor = Extractor { cmd: &cmd };
            extractor.run()?;

            let mut xbl_path = None;
            for entry in std::fs::read_dir(temp_dir.path())? {
                let entry = entry?;
                let p = entry.path();
                if p.is_dir() {
                    let candidate = p.join("xbl_config.img");
                    if candidate.exists() {
                        xbl_path = Some(candidate);
                        break;
                    }
                }
            }

            let xbl_path = xbl_path
                .ok_or_else(|| anyhow::anyhow!("xbl_config.img was not found in the payload!"))?;

            return match do_run(no_json, &xbl_path, path, metadata) {
                Ok(()) => Ok(()),
                Err(e) => anyhow::bail!("{}", e),
            };
        } else {
            // Treat as EDL firmware or other zip firmware
            println!("[arbscan] EDL firmware zip detected. Scanning for bootloader image...");

            // Look for candidates
            let mut candidate_name = None;
            let mut best_priority = usize::MAX;

            for name in archive.file_names() {
                let name_lower = name.to_lowercase();

                let matches_candidate = |suffix: &str| -> bool {
                    name_lower == suffix
                        || name_lower.ends_with(&format!("/{}", suffix))
                        || name_lower.ends_with(&format!("\\{}", suffix))
                };

                let priority = if matches_candidate("xbl_config.img") {
                    1
                } else if matches_candidate("xbl_config.elf") {
                    2
                } else if matches_candidate("xbl.img") {
                    3
                } else if matches_candidate("xbl.elf") {
                    4
                } else {
                    0
                };

                if priority > 0 && priority < best_priority {
                    best_priority = priority;
                    candidate_name = Some(name.to_string());
                }
            }

            let candidate_name = candidate_name.ok_or_else(|| {
                anyhow::anyhow!("No valid bootloader candidate (xbl_config.img/elf, xbl.img/elf) found in the zip archive!")
            })?;

            println!(
                "[arbscan] Found candidate: {}. Extracting temporarily...",
                candidate_name
            );
            let temp_dir = tempfile::tempdir()?;
            let temp_file_path = temp_dir.path().join("extracted_bootloader.img");

            {
                let mut temp_file = File::create(&temp_file_path)?;
                let mut entry = archive.by_name(&candidate_name)?;
                std::io::copy(&mut entry, &mut temp_file)?;
            }

            return match do_run(no_json, &temp_file_path, path, metadata) {
                Ok(()) => Ok(()),
                Err(e) => anyhow::bail!("{}", e),
            };
        }
    }

    anyhow::bail!(
        "Unsupported file format or invalid magic bytes. Only ELF images, OTA zip files, or EDL/firmware zip packages/directories are supported."
    );
}

fn do_run(
    no_json: bool,
    path: &Path,
    original_path: &Path,
    metadata: Option<std::collections::HashMap<String, String>>,
) -> Result<(), ArbError> {
    let mut file = File::open(path)?;

    let mut ehdr = [0u8; 64];
    file.read_exact(&mut ehdr)?;

    let valid_magic = matches!(ehdr, [0x7f, b'E', b'L', b'F', ..]);
    if !valid_magic || ehdr[EI_CLASS] != ELFCLASS64 || ehdr[EI_DATA] != ELFDATA2LSB {
        return Err(ArbError::InvalidElf("Not a valid little-endian ELF64 file"));
    }

    let e_phoff = read_le64(&ehdr, 0x20).ok_or(ArbError::InvalidElf("Truncated EHDR"))?;
    let e_phentsz = read_le16(&ehdr, 0x36).unwrap_or(0) as usize;
    let e_phnum = read_le16(&ehdr, 0x38).unwrap_or(0) as usize;

    // Minimum check for an ELF64 Program Header size (usually 56 bytes)
    if e_phentsz < 56 || e_phnum == 0 {
        return Err(ArbError::InvalidElf("Unexpected program header layout"));
    }

    let file_size = file.metadata()?.len();

    // Read all program headers at once
    let ph_table_size = e_phnum * e_phentsz;
    if ph_table_size > 65536 {
        return Err(ArbError::InvalidElf("Program header table too large"));
    }

    let mut ph_buf = vec![0u8; ph_table_size];
    file.seek(SeekFrom::Start(e_phoff))?;
    file.read_exact(&mut ph_buf)?;

    // Collect non-exec segment candidates
    let mut candidates = Vec::<(u64, u64)>::new();

    for i in 0..e_phnum {
        let off = i * e_phentsz;
        let Some(buf) = ph_buf.get(off..off + e_phentsz) else {
            break;
        };

        let Some(p_flags) = read_le32(buf, 4) else {
            continue;
        };
        let Some(p_offset) = read_le64(buf, 8) else {
            continue;
        };
        let Some(p_filesz) = read_le64(buf, 32) else {
            continue;
        };

        if p_filesz == 0 || p_offset + p_filesz > file_size {
            continue;
        }

        // Must be non-executable, big enough for hash header, under 20MB limit
        if (p_flags & 0x1) == 0 && p_filesz >= HASH_HDR_SIZE as u64 && p_filesz <= MAX_SEGMENT_SIZE
        {
            candidates.push((p_offset, p_filesz));
        }
    }

    // Select the correct HASH segment
    let mut seg = None;
    let mut header_off = None;
    let mut hash_off = 0u64;
    let mut hash_size = 0u64;

    // Reuse buffer to prevent allocating multiple Vecs
    let mut shared_buf = Vec::new();

    for (off, size) in candidates {
        shared_buf.resize(size as usize, 0);
        file.seek(SeekFrom::Start(off))?;
        file.read_exact(&mut shared_buf)?;

        let Some(hdr) = find_hash_header(&shared_buf) else {
            continue;
        };

        let Some(common_sz) = read_le32(&shared_buf, hdr + 4) else {
            continue;
        };
        let Some(qti_sz) = read_le32(&shared_buf, hdr + 8) else {
            continue;
        };

        let oem_md_off = hdr + HASH_HDR_SIZE + common_sz as usize + qti_sz as usize;

        if oem_md_off + 12 > shared_buf.len() {
            continue;
        }

        let major = read_le32(&shared_buf, oem_md_off).unwrap_or(0);
        let minor = read_le32(&shared_buf, oem_md_off + 4).unwrap_or(0);
        let arb = read_le32(&shared_buf, oem_md_off + 8).unwrap_or(0);

        if sane_version(major) && sane_version(minor) && sane_arb(arb) {
            seg = Some(shared_buf.clone());
            header_off = Some(hdr);
            hash_off = off;
            hash_size = size;
            break;
        }
    }

    let seg = seg.ok_or(ArbError::MissingMetadata(
        "Valid OEM ARB metadata not found",
    ))?;
    let header_off = header_off.unwrap(); // We know this is Some if seg is Some

    let common_sz = read_le32(&seg, header_off + 4).unwrap_or(0);
    let qti_sz = read_le32(&seg, header_off + 8).unwrap_or(0);
    let oem_md_off = header_off + HASH_HDR_SIZE + common_sz as usize + qti_sz as usize;

    let major = read_le32(&seg, oem_md_off).unwrap_or(0);
    let minor = read_le32(&seg, oem_md_off + 4).unwrap_or(0);
    let arb = read_le32(&seg, oem_md_off + 8).unwrap_or(0);

    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    println!("[arbscan] Analyzing: {}\n", file_name);

    println!("OEM Metadata");
    println!("────────────");
    println!("  Major Version : {}", major);
    println!("  Minor Version : {}", minor);
    println!("  ARB Index     : {}", arb);

    if !no_json && ask_yes_no("\nWrite JSON output? [y/N]: ") {
        let mut device_model = String::new();
        let mut update_label = String::new();
        let mut fully_auto = false;

        if let Some(meta) = &metadata {
            if let Some(ver) = meta.get("version_name").or(meta.get("ota_target_version")) {
                update_label = ver.clone();
                fully_auto = true;
            }
            if let Some(dev) = meta
                .get("post-device")
                .or(meta.get("pre-device"))
                .or(meta.get("product_name"))
                .or(meta.get("product_model"))
            {
                device_model = dev.clone();
            }
        }

        if fully_auto {
            // We have a solid OS version, skip the device model prompt entirely
            if !device_model.is_empty() {
                println!("Device model      : {}", device_model);
            }
            println!("Update / build    : {}", update_label);
        } else {
            // We couldn't confidently extract the version, ask the user
            if !device_model.is_empty() {
                let input = ask_string(&format!("Device model (default: {}): ", device_model));
                if !input.trim().is_empty() {
                    device_model = input.trim().to_string();
                }
            } else {
                device_model = ask_string("Device model      : ");
            }

            update_label = ask_string("Update / build    : ");
        }

        let meta = ArbMetadata {
            device_model,
            update_label,
            image: original_path.display().to_string(),
            major,
            minor,
            arb,
            hash_offset: hash_off,
            hash_size,
        };

        let out = json_filename(
            &meta.device_model,
            &meta.update_label,
            meta.arb,
            original_path,
        );
        write(&out, serde_json::to_string_pretty(&meta)?)?;
        println!("\n✔ JSON written: {}", out);
    }

    Ok(())
}
