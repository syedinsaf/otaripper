use anyhow::{Result, anyhow};
use nom::{
    bytes::complete::{tag, take},
    combinator::rest,
    number::complete::{be_u32, be_u64},
    IResult,
};

/// Chrome OS update payload format parser.
/// 
/// Update file format: contains all the operations needed to update a system to
/// a specific version. It can be a full payload which can update from any
/// version, or a delta payload which can only update from a specific version.
/// 
/// The binary format is:
/// - Magic bytes: "CrAU" (4 bytes)
/// - File format version (8 bytes, big-endian)
/// - Manifest size (8 bytes, big-endian)  
/// - [Optional] Metadata signature size (4 bytes, big-endian, only if version >= 2)
/// - Manifest data (variable length, protobuf serialized)
/// - [Optional] Metadata signature (variable length, only if version >= 2)
/// - Payload data (remaining bytes)
#[derive(Debug)]
#[allow(dead_code)]
pub struct Payload<'a> {
    /// Magic bytes identifier - should always be "CrAU".
    magic_bytes: &'a [u8],

    /// Major version of the payload file format.
    file_format_version: u64,

    /// Size in bytes of the manifest data that follows.
    manifest_size: u64,

    /// Size of the metadata signature in bytes.
    /// Only present if file_format_version >= 2.
    metadata_signature_size: Option<u32>,

    /// Serialized DeltaArchiveManifest protobuf message.
    /// Contains metadata about the update operations.
    pub manifest: &'a [u8],

    /// Cryptographic signature of the metadata (magic bytes through manifest).
    /// This is a serialized Signatures protobuf message.
    /// Only present if file_format_version >= 2.
    metadata_signature: Option<&'a [u8]>,

    /// Raw payload data containing the actual update content.
    /// The specific offset and length of each data blob is recorded in the manifest.
    pub data: &'a [u8],
}

impl<'a> Payload<'a> {
    /// Internal parser implementation using nom combinators.
    fn parse_inner(input: &'a [u8]) -> IResult<&'a [u8], Payload<'a>> {
        // Parse magic bytes - must be exactly "CrAU"
        let (input, magic_bytes) = tag(&b"CrAU"[..])(input)?;
        
        // Parse version and manifest size (both big-endian u64)
        let (input, file_format_version) = be_u64(input)?;
        let (input, manifest_size) = be_u64(input)?;
       
        // Metadata signature size only exists in version 2+
        let (input, metadata_signature_size) = if file_format_version > 1 {
            let (input, size) = be_u32(input)?;
            (input, Some(size))
        } else {
            (input, None)
        };
       
        // Parse manifest data (length determined by manifest_size)
        let (input, manifest) = take(manifest_size)(input)?;
       
        // Parse optional metadata signature
        let (input, metadata_signature) = match metadata_signature_size {
            Some(size) => {
                let (input, sig) = take(size)(input)?;
                (input, Some(sig))
            }
            None => (input, None),
        };
       
        // Everything remaining is payload data
        let (input, data) = rest(input)?;
       
        Ok((input, Payload {
            magic_bytes,
            file_format_version,
            manifest_size,
            metadata_signature_size,
            manifest,
            metadata_signature,
            data,
        }))
    }

    /// Parse a Chrome OS update payload from raw bytes.
    /// 
    /// # Arguments
    /// * `bytes` - Raw payload file data
    /// 
    /// # Returns
    /// * `Ok(Payload)` - Successfully parsed payload
    /// * `Err(anyhow::Error)` - Parse error with context
    /// 
    /// # Example
    /// ```rust
    /// let payload_data = std::fs::read("update.bin")?;
    /// let payload = Payload::parse(&payload_data)?;
    /// println!("Version: {}", payload.file_format_version);
    /// ```
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        match Self::parse_inner(bytes) {
            Ok((_, payload)) => Ok(payload),
            Err(e) => {
                Err(anyhow!("Failed to parse payload: {}", e))
                    .context("The payload file format is invalid or corrupted")
            }
        }
    }
}