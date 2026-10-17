//! Bounded original document bytes; no filenames, URLs, file IDs or text fallback.
//! Signature checks are not a PDF/image decoder or a claim about source quality.
use crate::application_run::{valid_digest, StructuredTextInput, ValidationError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

pub const STRUCTURED_DOCUMENT_INPUT_V1: &str = "structured_document_input_v1";
pub const MAX_DOCUMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentMediaType {
    #[serde(rename = "application/pdf")]
    Pdf,
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
}
impl DocumentMediaType {
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
    /// A fixed transport label, never the caller's private source filename.
    pub fn transport_filename(self) -> &'static str {
        match self {
            Self::Pdf => "document.pdf",
            Self::Png => "document.png",
            Self::Jpeg => "document.jpg",
        }
    }
    fn matches_signature(self, bytes: &[u8]) -> bool {
        match self {
            Self::Pdf => bytes.starts_with(b"%PDF-"),
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentLimits {
    pub max_input_bytes: usize,
    pub allowed_media_types: Vec<DocumentMediaType>,
}
impl DocumentLimits {
    pub fn valid(&self) -> bool {
        (12..=MAX_DOCUMENT_BYTES).contains(&self.max_input_bytes)
            && !self.allowed_media_types.is_empty()
            && self.allowed_media_types.len() <= 3
            && self
                .allowed_media_types
                .iter()
                .enumerate()
                .all(|(i, kind)| !self.allowed_media_types[..i].contains(kind))
    }
    pub fn allows(&self, source: &DocumentSource) -> bool {
        self.valid()
            && source.validate().is_ok()
            && source.source_bytes <= self.max_input_bytes
            && self.allowed_media_types.contains(&source.media_type)
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSource {
    pub source_sha256: String,
    pub source_bytes: usize,
    pub media_type: DocumentMediaType,
}
impl DocumentSource {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if !valid_digest(&self.source_sha256)
            || !(12..=MAX_DOCUMENT_BYTES).contains(&self.source_bytes)
        {
            return Err(ValidationError::InvalidRequest);
        }
        Ok(())
    }
}
impl fmt::Debug for DocumentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DocumentSource([private context omitted])")
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentInput {
    pub source: DocumentSource,
    pub output: StructuredTextInput,
}
impl DocumentInput {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.source.validate()?;
        self.output.validate()
    }
}
/// Transient original bytes. No Clone/Serialize; Debug reveals no content.
pub struct PreparedDocument {
    bytes: Vec<u8>,
    source: DocumentSource,
}
impl fmt::Debug for PreparedDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedDocument([private document omitted])")
    }
}
impl PreparedDocument {
    pub fn prepare(
        bytes: &[u8],
        media_type: DocumentMediaType,
        limits: &DocumentLimits,
    ) -> Result<Self, ValidationError> {
        if !limits.valid()
            || !limits.allowed_media_types.contains(&media_type)
            || bytes.len() < 12
            || bytes.len() > limits.max_input_bytes
            || !media_type.matches_signature(bytes)
        {
            return Err(ValidationError::InvalidRequest);
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            source: DocumentSource {
                source_sha256: format!("{:x}", Sha256::digest(bytes)),
                source_bytes: bytes.len(),
                media_type,
            },
        })
    }
    pub fn source(&self) -> &DocumentSource {
        &self.source
    }
    pub fn matches(&self, source: &DocumentSource) -> bool {
        source.validate().is_ok() && &self.source == source
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests;
