use anyhow::{Result, anyhow};
use nom_derive::{NomBE, Parse};

/// Update file format: contains all the operations needed to update a system to
/// a specific version. It can be a full payload which can update from any
/// version, or a delta payload which can only update from a specific version.
#[derive(Debug, NomBE)]
#[allow(dead_code)]
pub struct Payload<'a> {
    /// Should be "CrAU".
    #[nom(Tag = r#"b"CrAU""#)]
    magic_bytes: &'a [u8],

    /// Payload major version.
    file_format_version: u64,

    /// Size of [`DeltaArchiveManifest`].
    manifest_size: u64,

    // Only present if format_version >= 2.
    #[nom(If = "file_format_version > 1")]
    metadata_signature_size: Option<u32>,

    /// This is a serialized [`DeltaArchiveManifest`] message.
    #[nom(Take = "manifest_size")]
    pub manifest: &'a [u8],

    /// The signature of the metadata (from the beginning of the payload up to
    /// this location, not including the signature itself). This is a serialized
    /// [`Signatures`] message.
    #[nom(
        If = "metadata_signature_size.is_some()",
        Take = "metadata_signature_size.unwrap()"
    )]
    metadata_signature: Option<&'a [u8]>,

    /// Data blobs for files, no specific format. The specific offset and length
    /// of each data blob is recorded in the [`DeltaArchiveManifest`].
    #[nom(Parse = "::nom::combinator::rest")]
    pub data: &'a [u8],
}

impl<'a> Payload<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        // Parse the payload using the default error type
        match Parse::parse(bytes) {
            Ok((_, payload)) => Ok(payload),
            Err(e) => {
                // Provide a more descriptive error message
                Err(anyhow!("Failed to parse payload: {}", e)
                    .context("The payload file format is invalid or corrupted"))
            }
        }
    }
}
