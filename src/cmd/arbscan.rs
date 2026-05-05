use std::fmt;
use std::fs::{write, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;

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
    buf.get(off..off + 2)?.try_into().ok().map(u16::from_le_bytes)
}

fn read_le32(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)?.try_into().ok().map(u32::from_le_bytes)
}

fn read_le64(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..off + 8)?.try_into().ok().map(u64::from_le_bytes)
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

fn json_filename(input: &Path) -> String {
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    format!("{}_arb.json", stem)
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
        if hash_tbl_sz == 0 || (hash_tbl_sz & 0x1F) != 0 {
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
    match do_run(no_json, path) {
        Ok(()) => Ok(()),
        Err(e) => anyhow::bail!("{}", e),
    }
}

fn do_run(no_json: bool, path: &Path) -> Result<(), ArbError> {
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
        let Some(buf) = ph_buf.get(off..off + e_phentsz) else { break; };

        let Some(p_flags) = read_le32(buf, 4) else { continue; };
        let Some(p_offset) = read_le64(buf, 8) else { continue; };
        let Some(p_filesz) = read_le64(buf, 32) else { continue; };

        if p_filesz == 0 || p_offset + p_filesz > file_size {
            continue;
        }

        // Must be non-executable, big enough for hash header, under 20MB limit
        if (p_flags & 0x1) == 0
            && p_filesz >= HASH_HDR_SIZE as u64
            && p_filesz <= MAX_SEGMENT_SIZE
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

        let Some(common_sz) = read_le32(&shared_buf, hdr + 4) else { continue; };
        let Some(qti_sz) = read_le32(&shared_buf, hdr + 8) else { continue; };

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

    let seg = seg.ok_or(ArbError::MissingMetadata("Valid OEM ARB metadata not found"))?;
    let header_off = header_off.unwrap(); // We know this is Some if seg is Some

    let common_sz = read_le32(&seg, header_off + 4).unwrap_or(0);
    let qti_sz = read_le32(&seg, header_off + 8).unwrap_or(0);
    let oem_md_off = header_off + HASH_HDR_SIZE + common_sz as usize + qti_sz as usize;

    let major = read_le32(&seg, oem_md_off).unwrap_or(0);
    let minor = read_le32(&seg, oem_md_off + 4).unwrap_or(0);
    let arb = read_le32(&seg, oem_md_off + 8).unwrap_or(0);

    println!("[arbscan] Analyzing: {}\n", path.display());
    println!("OEM Metadata");
    println!("────────────");
    println!("  Major Version : {}", major);
    println!("  Minor Version : {}", minor);
    println!("  ARB Index     : {}", arb);

    if !no_json && ask_yes_no("\nWrite JSON output? [y/N]: ") {
        let device_model = ask_string("Device model      : ");
        let update_label = ask_string("Update / build    : ");

        let meta = ArbMetadata {
            device_model,
            update_label,
            image: path.display().to_string(),
            major,
            minor,
            arb,
            hash_offset: hash_off,
            hash_size,
        };

        let out = json_filename(path);
        write(&out, serde_json::to_string_pretty(&meta)?)?;
        println!("\n✔ JSON written: {}", out);
    }

    Ok(())
}
