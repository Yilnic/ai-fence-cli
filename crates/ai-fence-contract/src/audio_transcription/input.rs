//! Audio bytes are transient and deliberately have no Serialize/Debug/Clone.
use super::{TranscriptionError, MAX_INPUT_DURATION_MS};
use crate::application_run::valid_digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

pub const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioLimits {
    pub max_input_bytes: usize,
    pub max_input_duration_ms: u64,
}
impl AudioLimits {
    pub fn valid(&self) -> bool {
        (44..=MAX_AUDIO_BYTES).contains(&self.max_input_bytes)
            && (1..=MAX_INPUT_DURATION_MS).contains(&self.max_input_duration_ms)
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionInput {
    pub audio_sha256: String,
    pub source_duration_ms: u64,
    pub language: Option<String>,
    pub segment_timestamps: bool,
}
impl fmt::Debug for TranscriptionInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TranscriptionInput([private context omitted])")
    }
}
impl TranscriptionInput {
    pub fn validate(&self) -> Result<(), TranscriptionError> {
        if !valid_digest(&self.audio_sha256)
            || !(1..=MAX_INPUT_DURATION_MS).contains(&self.source_duration_ms)
            || self.language.as_ref().is_some_and(|language| {
                !(2..=3).contains(&language.len())
                    || !language.bytes().all(|byte| byte.is_ascii_lowercase())
            })
        {
            return Err(TranscriptionError::InvalidAudio);
        }
        Ok(())
    }
}
pub struct PreparedAudio {
    wav: Vec<u8>,
    duration_ms: u64,
    sha256: String,
}
impl fmt::Debug for PreparedAudio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedAudio([private audio omitted])")
    }
}
impl PreparedAudio {
    pub fn as_bytes(&self) -> &[u8] {
        &self.wav
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.wav
    }
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn matches(&self, input: &TranscriptionInput) -> bool {
        input.validate().is_ok()
            && input.source_duration_ms == self.duration_ms
            && input.audio_sha256 == self.sha256
    }
    pub fn prepare(input: &[u8], target: &AudioLimits) -> Result<Self, TranscriptionError> {
        let bad = TranscriptionError::InvalidAudio;
        if !target.valid()
            || input.len() > target.max_input_bytes
            || input.len() < 44
            || &input[..4] != b"RIFF"
            || &input[8..12] != b"WAVE"
        {
            return Err(bad);
        }
        let u16_at = |offset: usize| {
            input
                .get(offset..offset + 2)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u16::from_le_bytes)
        };
        let u32_at = |offset: usize| {
            input
                .get(offset..offset + 4)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_le_bytes)
        };
        if u32_at(4).map(u64::from) != Some((input.len() - 8) as u64) {
            return Err(bad);
        }
        let mut position = 12usize;
        let mut format = None;
        let mut audio = None;
        let mut chunks = 0;
        while position < input.len() {
            chunks += 1;
            if chunks > 64 || position + 8 > input.len() {
                return Err(bad);
            }
            let length = u32_at(position + 4).ok_or(bad)? as usize;
            let start = position.checked_add(8).ok_or(bad)?;
            let end = start.checked_add(length).ok_or(bad)?;
            let bytes = input.get(start..end).ok_or(bad)?;
            match &input[position..position + 4] {
                b"fmt " => {
                    if format.is_some()
                        || !matches!(length, 16 | 18)
                        || u16_at(start) != Some(1)
                        || (length == 18 && u16_at(start + 16) != Some(0))
                    {
                        return Err(bad);
                    }
                    let channels = u16_at(start + 2).ok_or(bad)?;
                    let rate = u32_at(start + 4).ok_or(bad)?;
                    let align = u16_at(start + 12).ok_or(bad)?;
                    if !(1..=2).contains(&channels)
                        || !(8000..=48000).contains(&rate)
                        || u16_at(start + 14) != Some(16)
                        || align != channels * 2
                        || u32_at(start + 8) != Some(rate * u32::from(align))
                    {
                        return Err(bad);
                    }
                    format = Some((bytes[..16].to_vec(), rate, align));
                }
                b"data" => {
                    if audio.replace(bytes).is_some() {
                        return Err(bad);
                    }
                }
                _ => {}
            }
            position = end.checked_add(length % 2).ok_or(bad)?;
            if position > input.len() {
                return Err(bad);
            }
        }
        let (format, rate, align) = format.ok_or(bad)?;
        let audio = audio.ok_or(bad)?;
        if audio.is_empty() || audio.len() % usize::from(align) != 0 {
            return Err(bad);
        }
        let samples = audio.len() as u64 / u64::from(align);
        let duration_ms = samples
            .checked_mul(1000)
            .ok_or(bad)?
            .div_ceil(u64::from(rate));
        if duration_ms > target.max_input_duration_ms {
            return Err(bad);
        }
        let mut wav = Vec::with_capacity(44 + audio.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + audio.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt \x10\0\0\0");
        wav.extend_from_slice(&format);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(audio.len() as u32).to_le_bytes());
        wav.extend_from_slice(audio);
        let sha256 = format!("{:x}", Sha256::digest(&wav));
        Ok(Self {
            wav,
            duration_ms,
            sha256,
        })
    }
}
