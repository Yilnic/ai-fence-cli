//! Bounded speech content, independent of transport, profiles and funding.
//! Transcript validity and reported billing duration are separate observations.
use crate::application_run::UniqueJson;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

mod input;
pub use input::{AudioLimits, PreparedAudio, TranscriptionInput, MAX_AUDIO_BYTES};
pub const AUDIO_TRANSCRIPTION_RUNS_V1: &str = "audio_transcription_runs_v1";
pub const DURATION_RUN_RECEIPTS_V3: &str = "duration_run_receipts_v3";

pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_INPUT_DURATION_MS: u64 = 900_000;
pub const MAX_SEGMENTS: usize = 4096;
const MAX_JSON_NODES: usize = 65_536;
const MAX_JSON_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptionError {
    InvalidResponse,
    InvalidAudio,
    TooLarge,
    InvalidTranscript,
    InvalidTimestamp,
    MissingSegments,
}
impl fmt::Display for TranscriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidAudio => "invalid bounded PCM16 WAV audio",
            Self::InvalidResponse => "invalid transcription response",
            Self::TooLarge => "transcription response exceeds its limits",
            Self::InvalidTranscript => "invalid transcription output",
            Self::InvalidTimestamp => "transcription timestamp is outside its source",
            Self::MissingSegments => "requested transcription segments are missing",
        })
    }
}
impl std::error::Error for TranscriptionError {}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionSegment {
    /// Original provider seconds; validation never clips or rewrites them.
    pub start: f64,
    pub end: f64,
    pub text: String,
}
impl fmt::Debug for TranscriptionSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TranscriptionSegment([redacted])")
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionOutput {
    pub text: String,
    pub language: Option<String>,
    /// Measured from the submitted audio, never inferred from a transcript.
    pub source_duration_ms: u64,
    /// None means timestamps were not returned; an empty array may mean silence.
    pub segments: Option<Vec<TranscriptionSegment>>,
}
impl fmt::Debug for TranscriptionOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TranscriptionOutput([redacted])")
    }
}
impl TranscriptionOutput {
    pub fn validate_for(
        &self,
        source_duration_ms: u64,
        require_segments: bool,
    ) -> Result<(), TranscriptionError> {
        if source_duration_ms == 0
            || source_duration_ms > MAX_INPUT_DURATION_MS
            || self.source_duration_ms != source_duration_ms
            || self.text.len() > MAX_RESPONSE_BYTES
            || self.language.as_ref().is_some_and(|language| {
                language.is_empty()
                    || language.len() > 128
                    || language.chars().any(char::is_control)
            })
        {
            return Err(TranscriptionError::InvalidTranscript);
        }
        let Some(segments) = &self.segments else {
            return if require_segments {
                Err(TranscriptionError::MissingSegments)
            } else {
                Ok(())
            };
        };
        if segments.len() > MAX_SEGMENTS {
            return Err(TranscriptionError::TooLarge);
        }
        if require_segments && segments.is_empty() && !self.text.trim().is_empty() {
            return Err(TranscriptionError::MissingSegments);
        }
        let mut previous_start = 0.0;
        let mut text_bytes = self.text.len();
        // Measured duration is rounded upward to milliseconds. Permit only
        // floating point noise at that boundary, not another millisecond.
        let end_limit = source_duration_ms as f64 / 1000.0;
        for segment in segments {
            if !segment.start.is_finite()
                || !segment.end.is_finite()
                || segment.start < previous_start
                || segment.end < segment.start
                || segment.end > end_limit + f64::EPSILON * end_limit.max(1.0)
            {
                return Err(TranscriptionError::InvalidTimestamp);
            }
            previous_start = segment.start;
            text_bytes = text_bytes.saturating_add(segment.text.len());
            if text_bytes > MAX_RESPONSE_BYTES {
                return Err(TranscriptionError::TooLarge);
            }
        }
        Ok(())
    }
}

pub struct TranscriptionObservation {
    pub output: Result<TranscriptionOutput, TranscriptionError>,
    /// Whole positive provider duration, rounded up to milliseconds. Independent
    /// of output validity and input limits; pricing/coverage belongs to admission.
    pub reported_duration_ms: Option<u64>,
}
impl fmt::Debug for TranscriptionObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptionObservation")
            .field("has_output", &self.output.is_ok())
            .field("has_duration", &self.reported_duration_ms.is_some())
            .finish_non_exhaustive()
    }
}

/// No unknown, token-based, mixed or malformed usage becomes zero duration.
pub fn reported_duration_ms(usage: &Value) -> Option<u64> {
    let usage = usage.as_object()?;
    if usage.get("type")?.as_str()? != "duration"
        || usage.keys().any(|key| key != "type" && key != "seconds")
    {
        return None;
    }
    let seconds = usage.get("seconds")?;
    if let Some(seconds) = seconds.as_u64() {
        return seconds.checked_mul(1000).filter(|duration| *duration > 0);
    }
    let milliseconds = (seconds.as_f64()? * 1000.0).ceil();
    // u64::MAX as f64 rounds to 2^64; a cast at that boundary would saturate.
    (milliseconds.is_finite() && milliseconds > 0.0 && milliseconds < u64::MAX as f64)
        .then_some(milliseconds as u64)
}

/// Strict whole-body parsing rejects duplicate keys, including ignored provider
/// extensions. Only independently valid usage survives invalid transcript output.
pub fn parse_response(
    bytes: &[u8],
    source_duration_ms: u64,
    require_segments: bool,
) -> Result<TranscriptionObservation, TranscriptionError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(TranscriptionError::TooLarge);
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let UniqueJson(value) =
        UniqueJson::deserialize(&mut decoder).map_err(|_| TranscriptionError::InvalidResponse)?;
    decoder
        .end()
        .map_err(|_| TranscriptionError::InvalidResponse)?;
    let mut stack = vec![(&value, 0usize)];
    let mut seen = 0usize;
    while let Some((item, depth)) = stack.pop() {
        seen += 1;
        if seen > MAX_JSON_NODES || depth > MAX_JSON_DEPTH {
            return Err(TranscriptionError::TooLarge);
        }
        let children = match item {
            Value::Object(values) => values.len(),
            Value::Array(values) => values.len(),
            _ => 0,
        };
        if seen + stack.len() + children > MAX_JSON_NODES {
            return Err(TranscriptionError::TooLarge);
        }
        match item {
            Value::Object(values) => stack.extend(values.values().map(|value| (value, depth + 1))),
            Value::Array(values) => stack.extend(values.iter().map(|value| (value, depth + 1))),
            _ => {}
        }
    }
    let object = value
        .as_object()
        .ok_or(TranscriptionError::InvalidResponse)?;
    let reported_duration_ms = object.get("usage").and_then(reported_duration_ms);
    let output = (|| {
        if object.get("error").is_some_and(|error| !error.is_null())
            || object.get("task").is_some_and(|task| task != "transcribe")
        {
            return Err(TranscriptionError::InvalidTranscript);
        }
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .ok_or(TranscriptionError::InvalidTranscript)?
            .to_owned();
        let language = object
            .get("language")
            .map(|language| {
                language
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(TranscriptionError::InvalidTranscript)
            })
            .transpose()?;
        let segments = object
            .get("segments")
            .map(|segments| {
                let segments = segments
                    .as_array()
                    .ok_or(TranscriptionError::InvalidTranscript)?;
                if segments.len() > MAX_SEGMENTS {
                    return Err(TranscriptionError::TooLarge);
                }
                segments
                    .iter()
                    .map(|segment| {
                        Ok(TranscriptionSegment {
                            start: segment
                                .get("start")
                                .and_then(Value::as_f64)
                                .ok_or(TranscriptionError::InvalidTimestamp)?,
                            end: segment
                                .get("end")
                                .and_then(Value::as_f64)
                                .ok_or(TranscriptionError::InvalidTimestamp)?,
                            text: segment
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or(TranscriptionError::InvalidTranscript)?
                                .to_owned(),
                        })
                    })
                    .collect::<Result<Vec<_>, TranscriptionError>>()
            })
            .transpose()?;
        let output = TranscriptionOutput {
            text,
            language,
            source_duration_ms,
            segments,
        };
        output.validate_for(source_duration_ms, require_segments)?;
        Ok(output)
    })();
    Ok(TranscriptionObservation {
        output,
        reported_duration_ms,
    })
}

#[cfg(test)]
mod tests;
