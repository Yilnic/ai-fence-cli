//! Creation fencing by an existing attempt key. This is not provider cancellation.
//! A capability name or DTO alone does not authorize execution or a refund.
use crate::{
    application::{BindingIdentity, SCHEMA_VERSION},
    application_run::{canonical_uuid, valid_digest, RunReason, RunStatus, ValidationError},
};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const APPLICATION_RUN_CANCELLATION_V1: &str = "application_run_cancellation_v1";
pub const MAX_CANCELLATION_REQUEST_BYTES: usize = 1024;
pub const MAX_CANCELLATION_RESPONSE_BYTES: usize = 8192;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRunRequest {
    pub schema_version: u32,
    pub payload_digest: String,
}
impl fmt::Debug for CancelRunRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelRunRequest")
            .field("schema_version", &self.schema_version)
            .finish_non_exhaustive()
    }
}
impl CancelRunRequest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != SCHEMA_VERSION || !valid_digest(&self.payload_digest) {
            return Err(ValidationError::InvalidRequest);
        }
        Ok(())
    }
}

pub fn parse_cancellation_request(bytes: &[u8]) -> Result<CancelRunRequest, ValidationError> {
    if bytes.len() > MAX_CANCELLATION_REQUEST_BYTES {
        return Err(ValidationError::TooLarge);
    }
    // Typed deserialization rejects duplicate as well as unknown fields. Do not
    // first deserialize through Value, which would lose duplicate-key evidence.
    let request: CancelRunRequest =
        serde_json::from_slice(bytes).map_err(|_| ValidationError::InvalidRequest)?;
    request.validate()?;
    Ok(request)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationOutcome {
    CreationPrevented,
    NotCancelled,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationRunObservation {
    pub run_id: String,
    pub attempt_id: String,
    pub status: RunStatus,
    pub reason: Option<RunReason>,
}
impl fmt::Debug for CancellationRunObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationRunObservation")
            .field("status", &self.status)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancellationReceipt {
    pub schema_version: u32,
    pub binding: BindingIdentity,
    pub binding_revision: u64,
    pub dispatch_key: String,
    pub payload_digest: String,
    pub outcome: CancellationOutcome,
    pub cancellation_id: Option<String>,
    pub observed_run: Option<CancellationRunObservation>,
}
impl fmt::Debug for RunCancellationReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunCancellationReceipt")
            .field("schema_version", &self.schema_version)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}
impl RunCancellationReceipt {
    pub fn validate(&self) -> Result<(), ValidationError> {
        let service = &self.binding.service_name;
        if self.schema_version != SCHEMA_VERSION
            || self.binding_revision == 0
            || self.binding_revision > i64::MAX as u64
            || !canonical_uuid(&self.binding.binding_id)
            || !canonical_uuid(&self.binding.application_id)
            || !canonical_uuid(&self.binding.stage_id)
            || !canonical_uuid(&self.dispatch_key)
            || !valid_digest(&self.payload_digest)
            || service.is_empty()
            || service.len() > 100
            || !service
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ValidationError::InvalidResponse);
        }
        if let Some(run) = &self.observed_run {
            let reason_valid = match run.status {
                RunStatus::Registered | RunStatus::Running | RunStatus::Completed => {
                    run.reason.is_none()
                }
                RunStatus::Failed | RunStatus::Rejected | RunStatus::Unknown => {
                    run.reason.is_some()
                }
            };
            if !canonical_uuid(&run.run_id) || !canonical_uuid(&run.attempt_id) || !reason_valid {
                return Err(ValidationError::InvalidResponse);
            }
        }
        let consistent = match self.outcome {
            CancellationOutcome::CreationPrevented => {
                self.cancellation_id.as_deref().is_some_and(canonical_uuid)
                    && self.observed_run.as_ref().is_none_or(|run| {
                        run.status == RunStatus::Rejected
                            && run.reason == Some(RunReason::AdmissionDenied)
                    })
            }
            CancellationOutcome::NotCancelled => {
                self.cancellation_id.is_none() && self.observed_run.is_some()
            }
        };
        if !consistent {
            return Err(ValidationError::InvalidResponse);
        }
        Ok(())
    }

    pub fn matches_attempt(&self, binding: &BindingIdentity, key: &str, digest: &str) -> bool {
        self.binding == *binding && self.dispatch_key == key && self.payload_digest == digest
    }
}

#[cfg(test)]
mod tests;
