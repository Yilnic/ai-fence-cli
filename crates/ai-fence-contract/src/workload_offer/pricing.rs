//! Disclosure of the actual immutable pricing snapshot. Consumers display or
//! freeze it; producer admission and settlement remain the financial authority.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "unit", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfferPricing {
    Tokens { snapshot: TokenPricing },
    Duration { snapshot: DurationPricing },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPricing {
    pub schema_version: u32,
    pub input: f64,
    pub cached_input: f64,
    pub cache_write: f64,
    pub output: f64,
    pub tiers: Vec<TokenTier>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenTier {
    /// Canonical decimal u64: browser consumers must not round large thresholds.
    pub threshold_tokens: String,
    pub input: f64,
    pub cached_input: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: f64,
    /// false = all-token pricing; true = marginal intervals, as frozen.
    pub marginal: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurationPricing {
    pub schema_version: u32,
    pub usd_per_minute: f64,
    pub billing_quantum_ms: u64,
    pub minimum_billable_ms: u64,
}
fn rate(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
impl TokenPricing {
    pub fn valid(&self) -> bool {
        self.schema_version == 2
            && [self.input, self.cached_input, self.cache_write, self.output]
                .into_iter()
                .all(rate)
            && self.tiers.len() <= 64
            && self.tiers.iter().all(|tier| {
                tier.threshold_tokens
                    .parse::<u64>()
                    .is_ok_and(|n| n.to_string() == tier.threshold_tokens)
                    && rate(tier.input)
                    && rate(tier.output)
                    && tier.cached_input.is_none_or(rate)
                    && tier.cache_write.is_none_or(rate)
            })
    }
}
impl DurationPricing {
    pub fn valid(&self) -> bool {
        self.schema_version == 1
            && rate(self.usd_per_minute)
            && (1..=60_000).contains(&self.billing_quantum_ms)
            && self.minimum_billable_ms <= crate::audio_transcription::MAX_INPUT_DURATION_MS
    }
}
