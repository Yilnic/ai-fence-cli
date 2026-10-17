//! Explicitly negotiated receipt v2 uses decimal strings for every counter.
//! Keep v1's numeric wire and closed field set unchanged.
use super::*;
use serde::Serializer;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Counter {
    Number(u64),
    Decimal(String),
}
#[derive(Default)]
enum Member<T> {
    #[default]
    Absent,
    Present(Option<T>),
}
impl<T> Member<T> {
    fn absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}
impl<T: Serialize> Serialize for Member<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent => None::<T>.serialize(serializer),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Member<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    provider_call_id: String,
    binding: BindingIdentity,
    binding_revision: u64,
    profile_id: String,
    profile_revision: u64,
    policy_revision: u64,
    selected_model: String,
    usage_state: UsageState,
    input_tokens: Option<Counter>,
    output_tokens: Option<Counter>,
    cached_input_tokens: Option<Counter>,
    reasoning_tokens: Option<Counter>,
    price_state: PriceState,
    usd_amount: Option<String>,
    price_snapshot_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Member::absent")]
    total_tokens: Member<Counter>,
    #[serde(default, skip_serializing_if = "Member::absent")]
    cache_write_tokens: Member<Counter>,
    #[serde(default, skip_serializing_if = "Member::absent")]
    settlement_state: Member<SettlementState>,
    #[serde(default, skip_serializing_if = "Member::absent")]
    input_audio_ms: Member<Counter>,
}
fn encoded(value: Option<u64>, version: u32) -> Option<Counter> {
    value.map(|n| {
        if version >= 2 {
            Counter::Decimal(n.to_string())
        } else {
            Counter::Number(n)
        }
    })
}
fn decoded(value: Option<Counter>, version: u32) -> Result<Option<u64>, &'static str> {
    value
        .map(|counter| match (version, counter) {
            (1, Counter::Number(n)) => Ok(n),
            (2 | 3, Counter::Decimal(n)) if canonical_counter(&n) => {
                n.parse().map_err(|_| "invalid receipt counter")
            }
            _ => Err("invalid receipt counter encoding"),
        })
        .transpose()
}
fn canonical_counter(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn extra<T>(value: Member<T>, version: u32) -> Result<Option<T>, &'static str> {
    match (version, value) {
        (1, Member::Absent) => Ok(None),
        (2, Member::Present(value)) => Ok(value),
        _ => Err("invalid receipt version fields"),
    }
}
impl Serialize for RunReceipt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let version = self.schema_version;
        if !matches!(version, 1..=3)
            || (version == 3
                && [
                    self.input_tokens,
                    self.output_tokens,
                    self.cached_input_tokens,
                    self.reasoning_tokens,
                    self.total_tokens,
                    self.cache_write_tokens,
                ]
                .iter()
                .any(Option::is_some))
            || (version == 1
                && (self.total_tokens.is_some()
                    || self.cache_write_tokens.is_some()
                    || self.settlement_state.is_some()))
        {
            return Err(serde::ser::Error::custom("invalid receipt version fields"));
        }
        Wire {
            schema_version: version,
            run_id: self.run_id.clone(),
            attempt_id: self.attempt_id.clone(),
            provider_call_id: self.provider_call_id.clone(),
            binding: self.binding.clone(),
            binding_revision: self.binding_revision,
            profile_id: self.profile_id.clone(),
            profile_revision: self.profile_revision,
            policy_revision: self.policy_revision,
            selected_model: self.selected_model.clone(),
            usage_state: self.usage_state,
            input_tokens: encoded(self.input_tokens, version),
            output_tokens: encoded(self.output_tokens, version),
            cached_input_tokens: encoded(self.cached_input_tokens, version),
            reasoning_tokens: encoded(self.reasoning_tokens, version),
            price_state: self.price_state,
            usd_amount: self.usd_amount.clone(),
            price_snapshot_ref: self.price_snapshot_ref.clone(),
            total_tokens: if version == 2 {
                Member::Present(encoded(self.total_tokens, version))
            } else {
                Member::Absent
            },
            cache_write_tokens: if version == 2 {
                Member::Present(encoded(self.cache_write_tokens, version))
            } else {
                Member::Absent
            },
            settlement_state: if version >= 2 {
                Member::Present(self.settlement_state)
            } else {
                Member::Absent
            },
            input_audio_ms: if version == 3 {
                Member::Present(encoded(self.input_audio_ms, version))
            } else if self.input_audio_ms.is_none() {
                Member::Absent
            } else {
                return Err(serde::ser::Error::custom("invalid receipt version fields"));
            },
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for RunReceipt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        let version = wire.schema_version;
        if !matches!(version, 1..=3) {
            return Err(de::Error::custom("unsupported receipt version"));
        }
        let token_version = if version == 3 { 1 } else { version };
        let total = extra(wire.total_tokens, token_version).map_err(de::Error::custom)?;
        let write = extra(wire.cache_write_tokens, token_version).map_err(de::Error::custom)?;
        let audio_version = if version == 3 { 2 } else { 1 };
        let audio = extra(wire.input_audio_ms, audio_version).map_err(de::Error::custom)?;
        Ok(Self {
            schema_version: version,
            run_id: wire.run_id,
            attempt_id: wire.attempt_id,
            provider_call_id: wire.provider_call_id,
            binding: wire.binding,
            binding_revision: wire.binding_revision,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            policy_revision: wire.policy_revision,
            selected_model: wire.selected_model,
            usage_state: wire.usage_state,
            input_audio_ms: decoded(audio, version).map_err(de::Error::custom)?,
            input_tokens: decoded(wire.input_tokens, version).map_err(de::Error::custom)?,
            output_tokens: decoded(wire.output_tokens, version).map_err(de::Error::custom)?,
            cached_input_tokens: decoded(wire.cached_input_tokens, version)
                .map_err(de::Error::custom)?,
            reasoning_tokens: decoded(wire.reasoning_tokens, version).map_err(de::Error::custom)?,
            total_tokens: decoded(total, version).map_err(de::Error::custom)?,
            cache_write_tokens: decoded(write, version).map_err(de::Error::custom)?,
            settlement_state: extra(wire.settlement_state, version.min(2))
                .map_err(de::Error::custom)?,
            price_state: wire.price_state,
            usd_amount: wire.usd_amount,
            price_snapshot_ref: wire.price_snapshot_ref,
        })
    }
}

pub(super) fn finite_decimal_usd(value: &str) -> bool {
    if value.len() > 635 {
        return false;
    }
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, None), |(w, f)| (w, Some(f)));
    if whole.is_empty()
        || whole.len() > 309
        || (whole != "0" && whole.starts_with('0'))
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|f| {
            f.is_empty() || f.len() > 324 || !f.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return false;
    }
    value.parse::<f64>().is_ok_and(|amount| {
        amount.is_finite() && (amount > 0.0 || !value.bytes().any(|b| matches!(b, b'1'..=b'9')))
    })
}
