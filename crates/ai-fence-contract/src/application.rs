//! Managed application connections. Provisioning is separate from inference.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const BINDING_READINESS_V1: &str = "application_binding_readiness_v1";
pub const APPLICATION_RUNS_V1: &str = "application_runs_v1";
pub const METADATA_ONLY_V1: &str = "metadata_only_retention_v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingIdentity {
    pub binding_id: String,
    pub application_id: String,
    pub stage_id: String,
    pub service_name: String,
}

/// Terminal revocation of one binding ID, including a registration whose reply
/// was lost. This administrative request confers no inference authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeBindingRequest {
    pub schema_version: u32,
    pub binding: BindingIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingRevocationReceipt {
    pub schema_version: u32,
    pub binding: BindingIdentity,
    pub revocation_id: String,
    pub revoked_at: chrono::DateTime<chrono::Utc>,
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
        && value
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f'))
}

fn revocation_identity(identity: &BindingIdentity) -> Result<(), &'static str> {
    if !canonical_uuid(&identity.binding_id)
        || !canonical_uuid(&identity.application_id)
        || !canonical_uuid(&identity.stage_id)
    {
        return Err("canonical non-nil binding/application/stage UUID required");
    }
    role(&identity.service_name)
}

impl RevokeBindingRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported revocation schema");
        }
        revocation_identity(&self.binding)
    }
}

impl BindingRevocationReceipt {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION
            || !canonical_uuid(&self.revocation_id)
            || self.revoked_at.timestamp() <= 0
        {
            return Err("invalid revocation receipt");
        }
        revocation_identity(&self.binding)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationBindingSpec {
    pub application_id: String,
    pub stage_id: String,
    pub service_name: String,
    pub issuer: String,
    pub client_id: String,
    pub audience: String,
    pub scope: String,
    pub consumer_origin: String,
    /// Application workload role -> operator-selected profile ID, never a model.
    pub profiles: BTreeMap<String, String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileBindingRequest {
    pub schema_version: u32,
    /// Zero creates; an identical retry returns the existing revision unchanged.
    pub expected_revision: u64,
    pub spec: ApplicationBindingSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationBinding {
    pub schema_version: u32,
    pub binding_id: String,
    pub revision: u64,
    pub spec: ApplicationBindingSpec,
}

impl ApplicationBinding {
    pub fn identity(&self) -> BindingIdentity {
        BindingIdentity {
            binding_id: self.binding_id.clone(),
            application_id: self.spec.application_id.clone(),
            stage_id: self.spec.stage_id.clone(),
            service_name: self.spec.service_name.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION || self.revision == 0 {
            return Err("unsupported binding schema or revision");
        }
        identifier(&self.binding_id)?;
        self.spec.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileBindingResponse {
    pub binding: ApplicationBinding,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAuthentication {
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub audience: String,
    pub scope: String,
    /// A deployment reference, not a credential value.
    pub client_secret_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileReference {
    pub id: String,
    /// None until the producer resolves an implemented profile revision.
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedConnection {
    pub schema_version: u32,
    pub fence_url: String,
    pub binding: BindingIdentity,
    pub binding_revision: u64,
    pub auth: ServiceAuthentication,
    pub profiles: BTreeMap<String, ProfileReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    BindingDisabled,
    RunExecutionNotImplemented,
    ProfileUnavailable,
    RequiredCapabilityMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadReadiness {
    pub profile_id: String,
    pub profile_revision: Option<u64>,
    pub ready: bool,
    pub reason: Option<UnavailableReason>,
    pub enforced_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationCapabilities {
    pub schema_version: u32,
    pub binding: BindingIdentity,
    pub binding_revision: u64,
    pub enabled: bool,
    pub enforced_capabilities: Vec<String>,
    pub workloads: BTreeMap<String, WorkloadReadiness>,
}

impl ApplicationBindingSpec {
    pub fn validate(&self) -> Result<(), &'static str> {
        for value in [
            &self.application_id,
            &self.stage_id,
            &self.client_id,
            &self.audience,
            &self.scope,
        ] {
            identifier(value)?;
        }
        role(&self.service_name)?;
        bounded(&self.issuer, 2048)?;
        bounded(&self.consumer_origin, 2048)?;
        if self.profiles.is_empty() || self.profiles.len() > 32 {
            return Err("one to 32 workload profiles required");
        }
        for (key, value) in &self.profiles {
            role(key)?;
            identifier(value)?;
        }
        Ok(())
    }
}

impl ManagedConnection {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION || self.binding_revision == 0 {
            return Err("unsupported connection schema or revision");
        }
        identifier(&self.binding.binding_id)?;
        let spec = ApplicationBindingSpec {
            application_id: self.binding.application_id.clone(),
            stage_id: self.binding.stage_id.clone(),
            service_name: self.binding.service_name.clone(),
            issuer: self.auth.issuer.clone(),
            client_id: self.auth.client_id.clone(),
            audience: self.auth.audience.clone(),
            scope: self.auth.scope.clone(),
            consumer_origin: self.fence_url.clone(),
            profiles: self
                .profiles
                .iter()
                .map(|(key, value)| (key.clone(), value.id.clone()))
                .collect(),
            enabled: true,
        };
        spec.validate()?;
        bounded(&self.auth.token_endpoint, 2048)?;
        bounded(&self.auth.client_secret_ref, 512)?;
        if !self.auth.client_secret_ref.starts_with("secret:")
            || self.auth.client_secret_ref.len() <= 7
            || self.auth.client_secret_ref.chars().any(char::is_whitespace)
            || self
                .profiles
                .values()
                .any(|profile| profile.revision == Some(0))
        {
            return Err("invalid profile revision or deployment secret reference");
        }
        Ok(())
    }
}

impl ApplicationCapabilities {
    pub fn matches(&self, expected: &ManagedConnection) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.binding == expected.binding
            && self.binding_revision == expected.binding_revision
            && self.workloads.len() == expected.profiles.len()
            && expected.profiles.iter().all(|(role, profile)| {
                self.workloads.get(role).is_some_and(|workload| {
                    workload.profile_id == profile.id
                        && profile
                            .revision
                            .is_none_or(|revision| workload.profile_revision == Some(revision))
                })
            })
    }

    pub fn supports_workload(&self, role: &str, required: &[&str]) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.enabled
            && self
                .enforced_capabilities
                .iter()
                .any(|value| value == APPLICATION_RUNS_V1)
            && self.workloads.get(role).is_some_and(|workload| {
                workload.ready
                    && workload.reason.is_none()
                    && workload
                        .profile_revision
                        .is_some_and(|revision| revision > 0)
                    && required.iter().all(|capability| {
                        self.enforced_capabilities
                            .iter()
                            .chain(&workload.enforced_capabilities)
                            .any(|value| value == capability)
                    })
            })
    }
}

fn bounded(value: &str, max: usize) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        Err("empty, oversized or control-bearing connection field")
    } else {
        Ok(())
    }
}

fn identifier(value: &str) -> Result<(), &'static str> {
    bounded(value, 512)?;
    if value.chars().any(char::is_whitespace) {
        Err("connection identifiers cannot contain whitespace")
    } else {
        Ok(())
    }
}

fn role(value: &str) -> Result<(), &'static str> {
    bounded(value, 100)?;
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err("invalid application workload or service name")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_connection_is_not_an_inference_capability() {
        let descriptor: ManagedConnection = serde_json::from_value(json!({
            "schema_version":1,"fence_url":"https://fence.example.test",
            "binding":{"binding_id":"binding","application_id":"app","stage_id":"stage","service_name":"ai"},
            "binding_revision":1,
            "auth":{"issuer":"https://id.example.test/","token_endpoint":"https://id.example.test/connect/token","client_id":"service-client","audience":"ai-service","scope":"app.stage.ai","client_secret_ref":"secret:identity.client.ai"},
            "profiles":{"review":{"id":"operator-document-profile","revision":null}}
        })).unwrap();
        descriptor.validate().unwrap();
        let mut capabilities = ApplicationCapabilities {
            schema_version: 1,
            binding: descriptor.binding.clone(),
            binding_revision: 1,
            enabled: true,
            enforced_capabilities: vec![BINDING_READINESS_V1.into()],
            workloads: BTreeMap::from([(
                "review".into(),
                WorkloadReadiness {
                    profile_id: "operator-document-profile".into(),
                    profile_revision: None,
                    ready: false,
                    reason: Some(UnavailableReason::RunExecutionNotImplemented),
                    enforced_capabilities: vec![],
                },
            )]),
        };
        assert!(capabilities.matches(&descriptor));
        assert!(!capabilities.supports_workload("review", &[METADATA_ONLY_V1]));
        capabilities
            .enforced_capabilities
            .extend([APPLICATION_RUNS_V1.into(), METADATA_ONLY_V1.into()]);
        let workload = capabilities.workloads.get_mut("review").unwrap();
        workload.profile_revision = Some(1);
        workload.ready = true;
        workload.reason = None;
        assert!(capabilities.supports_workload("review", &[METADATA_ONLY_V1]));
        capabilities.binding.stage_id = "foreign-stage".into();
        assert!(!capabilities.matches(&descriptor));
        capabilities.enabled = false;
        assert!(!capabilities.supports_workload("review", &[METADATA_ONLY_V1]));
    }

    #[test]
    fn missing_contract_fields_and_secret_values_are_rejected() {
        assert!(
            serde_json::from_value::<ApplicationCapabilities>(json!({"schema_version":1})).is_err()
        );
        assert!(serde_json::from_value::<ServiceAuthentication>(json!({
            "issuer":"https://id.example.test","token_endpoint":"https://id.example.test/token",
            "client_id":"a","audience":"b","scope":"c","client_secret_ref":"secret:ref",
            "client_secret":"must-not-enter-descriptors"
        }))
        .is_err());
    }
}
