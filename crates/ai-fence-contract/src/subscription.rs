//! Negotiation boundary, not subscription entitlement or budget enforcement.
use serde::{Deserialize, Serialize};

pub const MANAGED_SUBSCRIPTION_GATEWAY_V1: &str = "managed_subscription_gateway_v1";

/// Missing fields are errors: an old response must not deserialize as support.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayCapabilities {
    pub schema_version: u32,
    pub capabilities: Vec<String>,
}

impl GatewayCapabilities {
    /// Keep empty until actor binding, forwarding, budgets and fencing all exist.
    pub fn currently_enforced() -> Self {
        Self {
            schema_version: 1,
            capabilities: Vec::new(),
        }
    }

    pub fn supports_managed_subscription(&self) -> bool {
        self.schema_version == 1
            && self
                .capabilities
                .iter()
                .any(|capability| capability == MANAGED_SUBSCRIPTION_GATEWAY_V1)
    }
}

/// Required semantics must survive deserialization of key issuance. They are
/// deliberately rejected by the current producer, not stored as proof of support.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayIssueRequirements {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuance_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CreateGatewayKeyRequest;
    use serde_json::json;

    #[test]
    fn capabilities_do_not_advertise_unenforced_support() {
        let current = GatewayCapabilities::currently_enforced();
        assert!(!current.supports_managed_subscription());
        assert_eq!(
            serde_json::to_value(current).unwrap(),
            json!({
                "schema_version": 1, "capabilities": []
            })
        );
        for value in [
            json!({}),
            json!({"schema_version": 1}),
            json!({
                "schema_version": 1, "capabilities": null
            }),
        ] {
            assert!(serde_json::from_value::<GatewayCapabilities>(value).is_err());
        }
        for (schema_version, name, expected) in [
            (1, MANAGED_SUBSCRIPTION_GATEWAY_V1, true),
            (2, MANAGED_SUBSCRIPTION_GATEWAY_V1, false),
            (1, "managed_subscription_gateway_v2", false),
            (1, " managed_subscription_gateway_v1", false),
        ] {
            assert_eq!(
                GatewayCapabilities {
                    schema_version,
                    capabilities: vec![name.into()]
                }
                .supports_managed_subscription(),
                expected
            );
        }
    }

    #[test]
    fn required_semantics_are_not_silently_dropped() {
        for value in [
            json!({"required_capabilities": [MANAGED_SUBSCRIPTION_GATEWAY_V1]}),
            json!({"required_capabilities": ["bounded_run_budget_v1"]}),
            json!({"required_capabilities": ["unknown"]}),
            json!({"required_capabilities": [""]}),
            json!({"issuance_id": "retry-must-not-create-another-key"}),
            json!({"issuance_id": ""}),
            json!({"metadata": {"funding_binding": {"auth_lane": "subscription"}}}),
            json!({"metadata": {"funding_binding": {}}}),
            json!({"metadata": {"funding_binding": false}}),
        ] {
            let body: CreateGatewayKeyRequest = serde_json::from_value(value.clone()).unwrap();
            assert!(body.requires_unavailable_semantics(), "{value}");
            let roundtrip = serde_json::from_value::<CreateGatewayKeyRequest>(
                serde_json::to_value(body).unwrap(),
            )
            .unwrap();
            assert!(roundtrip.requires_unavailable_semantics());
        }
    }

    #[test]
    fn malformed_requirements_fail_deserialization() {
        for value in [
            json!({"required_capabilities": null}),
            json!({"required_capabilities": "managed_subscription_gateway_v1"}),
            json!({"required_capabilities": [7]}),
            json!({"issuance_id": 7}),
        ] {
            assert!(serde_json::from_value::<CreateGatewayKeyRequest>(value).is_err());
        }
    }

    #[test]
    fn legacy_keys_without_requirements_keep_their_wire_shape() {
        for value in [
            json!({}),
            json!({"required_capabilities": []}),
            json!({
                "metadata": {"funding_binding": null}, "issuance_id": null
            }),
        ] {
            let body: CreateGatewayKeyRequest = serde_json::from_value(value).unwrap();
            assert!(!body.requires_unavailable_semantics());
            let wire = serde_json::to_value(body).unwrap();
            assert!(wire.get("required_capabilities").is_none());
            assert!(wire.get("issuance_id").is_none());
            assert!(wire["metadata"].get("funding_binding").is_none());
        }
    }
}
