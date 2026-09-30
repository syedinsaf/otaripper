use anyhow::{Context, Result, anyhow, bail};

const PAYLOAD_MAGIC: &[u8] = b"CrAU";
const MAX_METADATA_SIG_SIZE: u32 = 64 * 1024 * 1024; // 64 MiB
const MAX_MANIFEST_SIZE: u64 = 256 * 1024 * 1024; // 256 MiB
const SUPPORTED_VERSION_MAX: u64 = 2;

#[derive(Debug)]
pub enum PayloadData<'a> {
    Local(&'a [u8]),
    #[cfg(feature = "remote")]
    Remote {
        url: String,
        data_offset: u64,
        client: reqwest::blocking::Client,
    },
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct Payload<'a> {
    pub file_format_version: u64,
    pub manifest_size: u64,
    pub manifest: &'a [u8],
    pub metadata_signature: Option<&'a [u8]>,
    pub data: PayloadData<'a>,
}

impl<'a> Payload<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        // ---- Basic Size Check ----
        if bytes.len() < 20 {
            bail!("This file is too small to be an Android update. Please check your download.");
        }

        // ---- Magic & Vibe Checks ----
        let magic = &bytes[0..4];
        if magic != PAYLOAD_MAGIC {
            let hint = match magic {
                m if m.starts_with(b"MZ") => {
                    "💀 Bro… you just fed me a WINDOWS .EXE.\nWhat do you want me to extract? Task Manager??\n\n"
                }
                b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08" => {
                    "📦 This is a ZIP archive… which is GREAT…\n…except it does NOT contain a valid payload.bin inside 😭\n\n"
                }
                b"\x7FELF" => {
                    "🐧 This is a Linux system file.\nI only extract Android updates, and this isn't one of them.\n\n"
                }
                m if m.starts_with(b"\xFF\xD8") => "🖼️ Not you trying to extract… a JPEG 💀\n\n",
                m if m.starts_with(b"\x89PNG") => {
                    "🖌️ This is a PNG image.\nPixels are not partitions my friend 😔\n\n"
                }
                _ => "❌ This file isn't a recognized Android update.\n\n",
            };

            bail!(
                "{hint}\
                👉 Valid inputs:\n  - A raw 'payload.bin' file\n  - A full OTA .zip (with payload.bin inside)\n\n\
                💡 Tip: Just drag the correct file onto otaripper! 😎",
            );
        }

        // ---- Version & Size Parsing ----
        let file_format_version = u64::from_be_bytes(
            *bytes
                .get(4..)
                .and_then(|b| b.first_chunk())
                .ok_or_else(|| anyhow!("Internal Error: Could not read version"))?,
        );

        if file_format_version > SUPPORTED_VERSION_MAX {
            bail!(
                "This update uses a newer format than this version of otaripper supports. Please check for an app update!"
            );
        }

        let manifest_size = u64::from_be_bytes(
            *bytes
                .get(12..)
                .and_then(|b| b.first_chunk())
                .ok_or_else(|| anyhow!("Internal Error: Could not read manifest size"))?,
        );

        if manifest_size > MAX_MANIFEST_SIZE {
            bail!("The update file metadata appears to be corrupted. Please try re-downloading.");
        }

        // ---- v2 Handling ----
        let (header_size, metadata_sig_size): (usize, usize) = if file_format_version >= 2 {
            if bytes.len() < 24 {
                bail!(
                    "The file header is incomplete. This usually happens with a broken download."
                );
            }
            let sig_size = u32::from_be_bytes(
                *bytes
                    .get(20..)
                    .and_then(|b| b.first_chunk())
                    .ok_or_else(|| anyhow!("Internal Error: Could not read signature"))?,
            );
            if sig_size > MAX_METADATA_SIG_SIZE {
                bail!("The file signature is invalid or corrupted.");
            }
            (24, sig_size as usize)
        } else {
            (20, 0)
        };

        // ---- Combined Bounds Check with Overflow Protection ----
        let manifest_len: usize = manifest_size
            .try_into()
            .context("This update is too large for your system memory to handle.")?;

        let data_start = header_size
            .checked_add(manifest_len)
            .and_then(|sum| sum.checked_add(metadata_sig_size))
            .ok_or_else(|| anyhow!("Memory overflow: This update file is abnormally large."))?;

        if data_start > bytes.len() {
            bail!(
                "❌ Extraction Failed\n\n\
                The file is missing a large chunk of data at the end. \n\
                👉 Your download was likely interrupted. Please try downloading the file again!"
            );
        }

        // ---- Final zero-copy slices ----
        Ok(Self {
            file_format_version,
            manifest_size,
            manifest: &bytes[header_size..header_size + manifest_len],
            metadata_signature: if metadata_sig_size > 0 {
                Some(&bytes[header_size + manifest_len..data_start])
            } else {
                None
            },
            data: PayloadData::Local(&bytes[data_start..]),
        })
    }

    #[cfg(feature = "remote")]
    pub fn parse_remote(
        manifest_buf: &'a [u8],
        url: String,
        payload_base_offset: u64,
        client: reqwest::blocking::Client,
    ) -> Result<Self> {
        if manifest_buf.len() < 20 {
            bail!("Remote header buffer is too short to be a valid payload.");
        }

        let magic = &manifest_buf[0..4];
        if magic != PAYLOAD_MAGIC {
            bail!("Remote payload magic mismatch.");
        }

        let file_format_version = u64::from_be_bytes(
            *manifest_buf
                .get(4..)
                .and_then(|b| b.first_chunk())
                .ok_or_else(|| anyhow!("Could not read remote payload format version"))?,
        );

        if file_format_version > SUPPORTED_VERSION_MAX {
            bail!("Remote update uses an unsupported format version.");
        }

        let manifest_size = u64::from_be_bytes(
            *manifest_buf
                .get(12..)
                .and_then(|b| b.first_chunk())
                .ok_or_else(|| anyhow!("Could not read remote manifest size"))?,
        );

        if manifest_size > MAX_MANIFEST_SIZE {
            bail!("Remote payload metadata size is corrupted or oversized.");
        }

        let (header_size, sig_size): (usize, usize) = if file_format_version >= 2 {
            if manifest_buf.len() < 24 {
                bail!("Remote header buffer is too short for format version 2.");
            }
            let sig_sz = u32::from_be_bytes(
                *manifest_buf
                    .get(20..)
                    .and_then(|b| b.first_chunk())
                    .ok_or_else(|| anyhow!("Could not read remote signature size"))?,
            );
            if sig_sz > MAX_METADATA_SIG_SIZE {
                bail!("Remote signature size is corrupted.");
            }
            (24, sig_sz as usize)
        } else {
            (20, 0)
        };

        let manifest_len: usize = manifest_size
            .try_into()
            .context("Remote manifest size exceeds memory limits.")?;

        let data_start = header_size
            .checked_add(manifest_len)
            .and_then(|sum| sum.checked_add(sig_size))
            .ok_or_else(|| anyhow!("Memory overflow in remote payload parsing."))?;

        if manifest_buf.len() < data_start {
            bail!("Remote manifest buffer is incomplete.");
        }

        Ok(Self {
            file_format_version,
            manifest_size,
            manifest: &manifest_buf[header_size..header_size + manifest_len],
            metadata_signature: if sig_size > 0 {
                Some(&manifest_buf[header_size + manifest_len..data_start])
            } else {
                None
            },
            data: PayloadData::Remote {
                url,
                data_offset: payload_base_offset
                    .checked_add(data_start as u64)
                    .ok_or_else(|| anyhow!("Overflow calculating remote payload data offset."))?,
                client,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payload_parse_short_input() {
        let short_bytes = [0u8; 10];
        assert!(Payload::parse(&short_bytes).is_err());
    }

    #[test]
    fn test_payload_parse_invalid_magic() {
        let mut exe_bytes = [0u8; 30];
        exe_bytes[0..2].copy_from_slice(b"MZ");
        let err = Payload::parse(&exe_bytes).unwrap_err().to_string();
        assert!(err.contains("WINDOWS .EXE"));

        let mut zip_bytes = [0u8; 30];
        zip_bytes[0..4].copy_from_slice(b"PK\x03\x04");
        let err = Payload::parse(&zip_bytes).unwrap_err().to_string();
        assert!(err.contains("ZIP archive"));
    }

    #[test]
    fn test_payload_parse_valid_header() {
        let mut buf = [0u8; 100];
        buf[0..4].copy_from_slice(b"CrAU");
        buf[4..12].copy_from_slice(&2u64.to_be_bytes());
        buf[12..20].copy_from_slice(&10u64.to_be_bytes());
        buf[20..24].copy_from_slice(&0u32.to_be_bytes());

        let payload = Payload::parse(&buf).unwrap();
        assert_eq!(payload.file_format_version, 2);
        assert_eq!(payload.manifest_size, 10);
        assert_eq!(payload.manifest.len(), 10);
    }

    #[test]
    fn test_payload_parse_unsupported_version() {
        let mut buf = [0u8; 100];
        buf[0..4].copy_from_slice(b"CrAU");
        buf[4..12].copy_from_slice(&99u64.to_be_bytes());

        assert!(Payload::parse(&buf).is_err());
    }
}
