//! Bounded structured-text attempts. This contract alone enables no execution.
//!
//! Dispatch keys identify one paid attempt, not a promise of result replay.
//! Registry metadata contains neither input nor result; callers persist a received
//! result locally. Use [`parse_request`] at an untrusted JSON boundary.
use crate::application::{BindingIdentity, SCHEMA_VERSION};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_RESULT_BYTES: usize = 512 * 1024;
pub const MAX_SCHEMA_BYTES: usize = 128 * 1024;
pub const MAX_PROMPT_BYTES: usize = 512 * 1024;
pub const MAX_INSTRUCTIONS_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    InvalidRequest,
    InvalidResponse,
    TooLarge,
    DigestMismatch,
    IdentityMismatch,
}
impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRequest => "invalid application run request",
            Self::InvalidResponse => "invalid application run response",
            Self::TooLarge => "application run exceeds size limit",
            Self::DigestMismatch => "application run payload digest mismatch",
            Self::IdentityMismatch => "application run identity mismatch",
        })
    }
}
impl std::error::Error for ValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Funding {
    ApplicationApi,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    MetadataOnly,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredTextInput {
    pub instructions: String,
    pub prompt: String,
    pub schema_name: String,
    pub json_schema: Value,
    pub max_output_tokens: u32,
}
impl fmt::Debug for StructuredTextInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StructuredTextInput([private content omitted])")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRunRequest {
    pub schema_version: u32,
    pub binding_revision: u64,
    pub workload_role: String,
    pub profile_id: String,
    pub profile_revision: u64,
    pub operation_id: String,
    pub attempt_number: u8,
    pub dispatch_key: String,
    pub payload_digest: String,
    pub funding: Funding,
    pub retention: Retention,
    pub input: StructuredTextInput,
}

pub fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || matches!(b, b'a'..=b'f')
            }
        })
        && value
            .bytes()
            .any(|b| matches!(b, b'1'..=b'9' | b'a'..=b'f'))
}
pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}
fn role(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn revision(value: u64) -> bool {
    value > 0 && value <= i64::MAX as u64
}

impl SubmitRunRequest {
    fn validate_fields(&self) -> Result<(), ValidationError> {
        if self.schema_version != SCHEMA_VERSION
            || !revision(self.binding_revision)
            || !revision(self.profile_revision)
            || !role(&self.workload_role)
            || !identifier(&self.profile_id)
            || !canonical_uuid(&self.operation_id)
            || !canonical_uuid(&self.dispatch_key)
            || !(1..=3).contains(&self.attempt_number)
            || !role(&self.input.schema_name)
            || self.input.prompt.is_empty()
            || !self.input.json_schema.is_object()
            || self.input.max_output_tokens == 0
        {
            return Err(ValidationError::InvalidRequest);
        }
        if self.input.instructions.len() > MAX_INSTRUCTIONS_BYTES
            || self.input.prompt.len() > MAX_PROMPT_BYTES
            || serde_json::to_vec(&self.input.json_schema)
                .map_err(|_| ValidationError::InvalidRequest)?
                .len()
                > MAX_SCHEMA_BYTES
            || serde_json::to_vec(self)
                .map_err(|_| ValidationError::InvalidRequest)?
                .len()
                > MAX_REQUEST_BYTES
        {
            return Err(ValidationError::TooLarge);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        self.validate_fields()?;
        if !valid_digest(&self.payload_digest) || self.payload_digest != self.canonical_digest()? {
            return Err(ValidationError::DigestMismatch);
        }
        Ok(())
    }

    /// SHA256 of canonical JSON of every field except dispatch_key/payload_digest.
    /// Objects are recursively key-sorted; arrays and serde JSON scalar encoding
    /// are preserved. Changing a revision, attempt, policy selector or input
    /// changes the digest. A new dispatch key never changes the same attempt.
    pub fn canonical_digest(&self) -> Result<String, ValidationError> {
        self.validate_fields()?;
        let mut value = serde_json::to_value(self).map_err(|_| ValidationError::InvalidRequest)?;
        let object = value
            .as_object_mut()
            .ok_or(ValidationError::InvalidRequest)?;
        object.remove("dispatch_key");
        object.remove("payload_digest");
        let bytes = canonical_bytes(&value)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

fn canonical_bytes(value: &Value) -> Result<Vec<u8>, ValidationError> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(values) => {
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_unstable();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&values[k])))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_vec(&sorted(value)).map_err(|_| ValidationError::InvalidRequest)
}

/// Reject duplicate keys at *every* depth, including the private JSON schema.
/// Generic serde_json::Value parsing otherwise silently chooses the last key.
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("unambiguous JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueJson(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(v)) = seq.next_element()? {
                    values.push(v);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((k, UniqueJson(v))) = map.next_entry::<String, UniqueJson>()? {
                    if values.insert(k, v).is_some() {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
pub fn parse_request(bytes: &[u8]) -> Result<SubmitRunRequest, ValidationError> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(ValidationError::TooLarge);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value =
        UniqueJson::deserialize(&mut deserializer).map_err(|_| ValidationError::InvalidRequest)?;
    deserializer
        .end()
        .map_err(|_| ValidationError::InvalidRequest)?;
    let request: SubmitRunRequest =
        serde_json::from_value(value.0).map_err(|_| ValidationError::InvalidRequest)?;
    request.validate()?;
    Ok(request)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Registered,
    Running,
    Completed,
    Failed,
    Rejected,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultDelivery {
    Inline,
    NotRetained,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunReason {
    ExecutionUnavailable,
    BindingDisabled,
    BindingRevisionChanged,
    ProfileUnavailable,
    ProfileRevisionChanged,
    AdmissionDenied,
    InvalidOutput,
    ProviderFailed,
    DispatchUnknown,
    InternalFailure,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageState {
    Unknown,
    Reported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceState {
    Unknown,
    Estimated,
    Settled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunReceipt {
    pub schema_version: u32,
    pub run_id: String,
    pub attempt_id: String,
    /// Fence-authored immutable provider-call UUID, never a raw provider header.
    pub provider_call_id: String,
    pub binding: BindingIdentity,
    pub binding_revision: u64,
    pub profile_id: String,
    pub profile_revision: u64,
    pub policy_revision: u64,
    pub selected_model: String,
    pub usage_state: UsageState,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub price_state: PriceState,
    /// Nonnegative bounded decimal USD string. Unknown is null, never zero.
    pub usd_amount: Option<String>,
    pub price_snapshot_ref: Option<String>,
}
impl RunReceipt {
    pub fn validate(&self) -> Result<(), ValidationError> {
        let counts = [
            self.input_tokens,
            self.output_tokens,
            self.cached_input_tokens,
            self.reasoning_tokens,
        ];
        if self.schema_version != SCHEMA_VERSION
            || !canonical_uuid(&self.run_id)
            || !canonical_uuid(&self.attempt_id)
            || !canonical_uuid(&self.provider_call_id)
            || !valid_binding(&self.binding)
            || !revision(self.binding_revision)
            || !revision(self.profile_revision)
            || !revision(self.policy_revision)
            || !identifier(&self.profile_id)
            || !identifier(&self.selected_model)
            || (self.usage_state == UsageState::Unknown && counts.iter().any(Option::is_some))
            || (self.usage_state == UsageState::Reported && counts.iter().all(Option::is_none))
            || (self.price_state == PriceState::Unknown
                && (self.usd_amount.is_some() || self.price_snapshot_ref.is_some()))
            || (self.price_state != PriceState::Unknown
                && (!self.usd_amount.as_deref().is_some_and(decimal_usd)
                    || !self.price_snapshot_ref.as_deref().is_some_and(identifier)))
        {
            return Err(ValidationError::InvalidResponse);
        }
        Ok(())
    }
}
fn decimal_usd(value: &str) -> bool {
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, None), |(w, f)| (w, Some(f)));
    !whole.is_empty()
        && whole.len() <= 18
        && (whole == "0" || !whole.starts_with('0'))
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction
            .is_none_or(|f| !f.is_empty() && f.len() <= 12 && f.bytes().all(|b| b.is_ascii_digit()))
}
fn valid_binding(binding: &BindingIdentity) -> bool {
    canonical_uuid(&binding.binding_id)
        && canonical_uuid(&binding.application_id)
        && canonical_uuid(&binding.stage_id)
        && role(&binding.service_name)
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResponse {
    pub schema_version: u32,
    pub run_id: String,
    pub attempt_id: String,
    pub binding: BindingIdentity,
    pub binding_revision: u64,
    pub operation_id: String,
    pub attempt_number: u8,
    pub dispatch_key: String,
    pub payload_digest: String,
    pub workload_role: String,
    pub profile_id: String,
    pub profile_revision: u64,
    pub policy_revision: u64,
    pub status: RunStatus,
    pub reason: Option<RunReason>,
    pub receipt: Option<RunReceipt>,
    pub result_delivery: ResultDelivery,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_json"
    )]
    pub result: Option<Value>,
}
fn present_json<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}
impl fmt::Debug for RunResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunResponse")
            .field("run_id", &self.run_id)
            .field("status", &self.status)
            .field("result_delivery", &self.result_delivery)
            .field("result", &"[private content omitted]")
            .finish_non_exhaustive()
    }
}
impl RunResponse {
    /// Check structural invariants before trusting any receipt or inline content.
    /// `allow_inline` is true ONLY for the authoritative submit response, never GET.
    pub fn validate(&self, allow_inline: bool) -> Result<(), ValidationError> {
        if self.schema_version != SCHEMA_VERSION
            || !valid_binding(&self.binding)
            || !canonical_uuid(&self.run_id)
            || !canonical_uuid(&self.attempt_id)
            || !canonical_uuid(&self.operation_id)
            || !canonical_uuid(&self.dispatch_key)
            || !valid_digest(&self.payload_digest)
            || !revision(self.binding_revision)
            || !revision(self.profile_revision)
            || !revision(self.policy_revision)
            || !identifier(&self.profile_id)
            || !role(&self.workload_role)
            || !(1..=3).contains(&self.attempt_number)
        {
            return Err(ValidationError::InvalidResponse);
        }
        if let Some(receipt) = &self.receipt {
            receipt.validate()?;
            if receipt.run_id != self.run_id
                || receipt.attempt_id != self.attempt_id
                || receipt.binding != self.binding
                || receipt.binding_revision != self.binding_revision
                || receipt.profile_id != self.profile_id
                || receipt.profile_revision != self.profile_revision
                || receipt.policy_revision != self.policy_revision
            {
                return Err(ValidationError::IdentityMismatch);
            }
        }
        match self.status {
            RunStatus::Completed => {
                if self.reason.is_some() || self.receipt.is_none() {
                    return Err(ValidationError::InvalidResponse);
                }
                match self.result_delivery {
                    ResultDelivery::Inline if allow_inline && self.result.is_some() => {}
                    ResultDelivery::NotRetained if self.result.is_none() => {}
                    _ => return Err(ValidationError::InvalidResponse),
                }
            }
            status => {
                if self.result_delivery != ResultDelivery::None
                    || self.result.is_some()
                    || (matches!(status, RunStatus::Registered | RunStatus::Running)
                        && self.reason.is_some())
                    || (matches!(
                        status,
                        RunStatus::Failed | RunStatus::Rejected | RunStatus::Unknown
                    ) && self.reason.is_none())
                    || (status == RunStatus::Rejected && self.receipt.is_some())
                {
                    return Err(ValidationError::InvalidResponse);
                }
            }
        }
        if let Some(result) = &self.result {
            if serde_json::to_vec(result)
                .map_err(|_| ValidationError::InvalidResponse)?
                .len()
                > MAX_RESULT_BYTES
            {
                return Err(ValidationError::TooLarge);
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| ValidationError::InvalidResponse)?
            .len()
            > MAX_RESPONSE_BYTES
        {
            return Err(ValidationError::TooLarge);
        }
        Ok(())
    }

    pub fn matches_request(&self, request: &SubmitRunRequest) -> bool {
        self.binding_revision == request.binding_revision
            && self.operation_id == request.operation_id
            && self.attempt_number == request.attempt_number
            && self.dispatch_key == request.dispatch_key
            && self.payload_digest == request.payload_digest
            && self.workload_role == request.workload_role
            && self.profile_id == request.profile_id
            && self.profile_revision == request.profile_revision
    }

    /// Strip an ephemeral result before returning duplicate POST/metadata GET.
    pub fn without_result(mut self) -> Self {
        self.result = None;
        self.result_delivery = if self.status == RunStatus::Completed {
            ResultDelivery::NotRetained
        } else {
            ResultDelivery::None
        };
        self
    }
}

#[cfg(test)]
mod tests;
