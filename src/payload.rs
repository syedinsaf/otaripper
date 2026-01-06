// payload.rs
use anyhow::{Result, anyhow, bail};

const PAYLOAD_MAGIC: &[u8] = b"CrAU";

#[derive(Debug)]
#[allow(dead_code)]
pub struct Payload<'a> {
    pub file_format_version: u64,
    pub manifest_size: u64,
    pub manifest: &'a [u8],
    pub metadata_signature: Option<&'a [u8]>,
    pub data: &'a [u8],
}

impl<'a> Payload<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        // ---- Minimum header sanity ----
        if bytes.len() < 20 {
            bail!(
                "Payload too short to contain base header (need at least 20 bytes, got {})",
                bytes.len()
            );
        }

        // ---- Magic ----
        let magic = &bytes[0..4];

        if magic != PAYLOAD_MAGIC {
            // Funny diagnostics because devs deserve joy
            let mut vibe = String::new();

            // Windows PE / EXE
            if magic.starts_with(b"MZ") {
                vibe.push_str("💀 Bro… you just fed me a WINDOWS .EXE.\n");
                vibe.push_str("What do you want me to extract? Task Manager??\n\n");
            }
            // ZIP
            else if magic == b"PK\x03\x04" || magic == b"PK\x05\x06" || magic == b"PK\x07\x08" {
                vibe.push_str("📦 This is a ZIP… which is GREAT…\n");
                vibe.push_str("…except it does NOT contain a valid payload.bin 😭\n\n");
            }
            // Linux ELF binary
            else if magic == b"\x7FELF" {
                vibe.push_str("🐧 This is an ELF binary.\n");
                vibe.push_str("You have given me Linux. I cannot extract Linux. I *am* Linux (spiritually).\n\n");
            }
            // JPEG
            else if magic.starts_with(b"\xFF\xD8") {
                vibe.push_str("🖼️ Not you trying to extract… a JPEG 💀\n\n");
            }
            // PNG
            else if magic.starts_with(b"\x89PNG") {
                vibe.push_str("🖌️ This is a PNG image.\n");
                vibe.push_str("Pixels are not partitions my friend 😔\n\n");
            }

            bail!(
                "{}Expected OTA payload header: 'CrAU'\n\
Found bytes: {:02X} {:02X} {:02X} {:02X}\n\n\
👉 Valid inputs:\n  - payload.bin\n  - OTA update .zip (with payload.bin inside)\n\n\
If unsure:\n  drag OTA.zip or payload.bin onto otaripper 😎",
                vibe,
                magic[0],
                magic[1],
                magic[2],
                magic[3]
            );
        }

        // ---- Version ----
        let file_format_version = u64::from_be_bytes(
            bytes[4..12]
                .try_into()
                .map_err(|_| anyhow!("Failed to read file format version"))?,
        );

        if file_format_version > 2 {
            bail!(
                "Unsupported payload version {} (only v1/v2 supported). Please update otaripper.",
                file_format_version
            );
        }

        // ---- Manifest Size ----
        let manifest_size = u64::from_be_bytes(
            bytes[12..20]
                .try_into()
                .map_err(|_| anyhow!("Failed to read manifest size"))?,
        );

        // ---- v2 signature size handling ----
        if file_format_version >= 2 && bytes.len() < 24 {
            bail!("Version 2 payload requires at least 24-byte header");
        }

        let metadata_sig_size_u32 = if file_format_version >= 2 {
            u32::from_be_bytes(
                bytes[20..24]
                    .try_into()
                    .map_err(|_| anyhow!("Failed to read metadata signature size"))?,
            )
        } else {
            0
        };

        if metadata_sig_size_u32 > 64 * 1024 * 1024 {
            bail!(
                "Metadata signature size {} bytes is unreasonably large",
                metadata_sig_size_u32
            );
        }

        let header_size: usize = if file_format_version >= 2 { 24 } else { 20 };

        // ---- Manifest bounds ----
        let manifest_start = header_size;
        let manifest_end = manifest_start
            .checked_add(manifest_size as usize)
            .ok_or_else(|| anyhow!("Manifest size overflow"))?;

        if manifest_end > bytes.len() {
            bail!("Declared manifest size exceeds payload length");
        }

        // ---- Signature + Data bounds ----
        let data_start = manifest_end
            .checked_add(metadata_sig_size_u32 as usize)
            .ok_or_else(|| anyhow!("Metadata signature size overflow"))?;

        if data_start > bytes.len() {
            bail!("Metadata signature extends beyond end of payload");
        }

        // ---- Final zero-copy slices ----
        let manifest = &bytes[manifest_start..manifest_end];
        let metadata_signature = if metadata_sig_size_u32 > 0 {
            Some(&bytes[manifest_end..data_start])
        } else {
            None
        };
        let data = &bytes[data_start..];

        Ok(Self {
            file_format_version,
            manifest_size,
            manifest,
            metadata_signature,
            data,
        })
    }
}
