//! Scoped processing disclosure for consent. This snapshot is never a lease,
//! dispatch authorization, quote reservation or proof of upstream retention.
use crate::{
    application::{BindingIdentity, ManagedConnection, FULL_RUN_RECEIPTS_V2, METADATA_ONLY_V1},
    application_run::{InputKind, Retention, MAX_REQUEST_BYTES},
    audio_transcription::{AudioLimits, AUDIO_TRANSCRIPTION_RUNS_V1, DURATION_RUN_RECEIPTS_V3},
    document_input::{DocumentLimits, STRUCTURED_DOCUMENT_INPUT_V1},
};
use serde::{Deserialize, Serialize};
use url::Url;

mod pricing;
pub use pricing::{DurationPricing, OfferPricing, TokenPricing, TokenTier};
pub const WORKLOAD_OFFER_V1: &str = "application_workload_offer_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    OperatorConfigured,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataPolicy {
    pub url: String,
    pub revision: String,
    pub source: PolicySource,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechOffer {
    pub limits: AudioLimits,
    pub segment_timestamps: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadOffer {
    pub schema_version: u32,
    pub binding: BindingIdentity,
    pub binding_revision: u64,
    pub workload_role: String,
    pub profile_id: String,
    pub profile_revision: u64,
    pub input_kind: InputKind,
    /// Canonical origin only: no credential-bearing API path, headers or keys.
    pub provider_origin: String,
    pub selected_model: String,
    pub data_policy: DataPolicy,
    /// Producer-side retention only. The linked upstream policy is operator
    /// configuration, not an assertion that Fence controls the provider.
    pub gateway_retention: Retention,
    pub max_request_bytes: u32,
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub documents: Option<DocumentLimits>,
    pub speech: Option<SpeechOffer>,
    /// Complete frozen rates/tiers, without granting funds or repricing a run.
    pub pricing: OfferPricing,
    pub maximum_estimated_call_usd: f64,
    pub enforced_capabilities: Vec<String>,
}
fn bounded(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}
pub fn valid_workload_role(value: &str) -> bool {
    bounded(value, 100)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
impl WorkloadOffer {
    pub fn protocol_capabilities(&self) -> [&'static str; 3] {
        let (protocol, receipt) = match self.input_kind {
            InputKind::StructuredText => (
                crate::application::APPLICATION_RUNS_V1,
                FULL_RUN_RECEIPTS_V2,
            ),
            InputKind::NativeFunctions => (
                crate::application_native::NATIVE_FUNCTION_TURNS_V1,
                FULL_RUN_RECEIPTS_V2,
            ),
            InputKind::StructuredDocument => (STRUCTURED_DOCUMENT_INPUT_V1, FULL_RUN_RECEIPTS_V2),
            InputKind::AudioTranscription => {
                (AUDIO_TRANSCRIPTION_RUNS_V1, DURATION_RUN_RECEIPTS_V3)
            }
        };
        [protocol, receipt, METADATA_ONLY_V1]
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        let invalid = || "invalid application workload offer";
        let origin = Url::parse(&self.provider_origin).map_err(|_| invalid())?;
        let policy = Url::parse(&self.data_policy.url).map_err(|_| invalid())?;
        let loopback = matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if self.schema_version != 1
            || self.binding_revision == 0
            || self.profile_revision == 0
            || !valid_workload_role(&self.workload_role)
            || !valid_workload_role(&self.binding.service_name)
            || [
                &self.binding.binding_id,
                &self.binding.application_id,
                &self.binding.stage_id,
                &self.profile_id,
            ]
            .iter()
            .any(|v| !bounded(v, 512))
            || !bounded(&self.selected_model, 512)
            || !bounded(&self.data_policy.revision, 128)
            || !bounded(&self.data_policy.url, 2048)
            || self.provider_origin.len() > 2048
            || self.data_policy.url.len() > 2048
            || !(origin.scheme() == "https" || (origin.scheme() == "http" && loopback))
            || origin.host_str().is_none()
            || origin.origin().ascii_serialization() != self.provider_origin
            || !origin.username().is_empty()
            || origin.password().is_some()
            || policy.scheme() != "https"
            || policy.host_str().is_none()
            || !policy.username().is_empty()
            || policy.password().is_some()
            || policy.query().is_some()
            || policy.fragment().is_some()
            || !(1024..=MAX_REQUEST_BYTES as u32).contains(&self.max_request_bytes)
            || !self.maximum_estimated_call_usd.is_finite()
            || !(0.0..=1_000_000.0).contains(&self.maximum_estimated_call_usd)
            || self.enforced_capabilities.len() > 16
            || self
                .enforced_capabilities
                .iter()
                .enumerate()
                .any(|(i, c)| !bounded(c, 100) || self.enforced_capabilities[..i].contains(c))
            || self
                .protocol_capabilities()
                .iter()
                .chain([&WORKLOAD_OFFER_V1])
                .any(|c| !self.enforced_capabilities.iter().any(|v| v == c))
        {
            return Err(invalid());
        }
        let [protocol, receipt, _] = self.protocol_capabilities();
        if self.enforced_capabilities.iter().any(|c| {
            [
                crate::application::APPLICATION_RUNS_V1,
                crate::application_native::NATIVE_FUNCTION_TURNS_V1,
                STRUCTURED_DOCUMENT_INPUT_V1,
                AUDIO_TRANSCRIPTION_RUNS_V1,
            ]
            .contains(&c.as_str())
                && c != protocol
                || [FULL_RUN_RECEIPTS_V2, DURATION_RUN_RECEIPTS_V3].contains(&c.as_str())
                    && c != receipt
        }) {
            return Err(invalid());
        }
        let valid = match self.input_kind {
            InputKind::AudioTranscription => {
                self.documents.is_none()
                    && self.max_input_tokens == 0
                    && self.max_output_tokens == 0
                    && self.speech.as_ref().is_some_and(|s| s.limits.valid())
                    && matches!(&self.pricing, OfferPricing::Duration { snapshot } if snapshot.valid())
            }
            kind => {
                self.speech.is_none()
                    && (1..=1_048_576).contains(&self.max_input_tokens)
                    && (1..=65_536).contains(&self.max_output_tokens)
                    && if kind == InputKind::StructuredDocument {
                        self.documents.as_ref().is_some_and(DocumentLimits::valid)
                    } else {
                        self.documents.is_none()
                            && self.max_input_tokens >= self.max_request_bytes.saturating_add(4096)
                    }
                    && matches!(&self.pricing, OfferPricing::Tokens { snapshot } if snapshot.valid())
            }
        };
        if !valid {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn matches(&self, connection: &ManagedConnection, role: &str, revision: u64) -> bool {
        self.validate().is_ok()
            && connection.validate().is_ok()
            && self.binding == connection.binding
            && self.binding_revision == connection.binding_revision
            && self.workload_role == role
            && self.profile_revision == revision
            && connection.profiles.get(role).is_some_and(|p| {
                p.id == self.profile_id && p.revision.is_none_or(|r| r == revision)
            })
    }
}
#[cfg(test)]
mod tests;
