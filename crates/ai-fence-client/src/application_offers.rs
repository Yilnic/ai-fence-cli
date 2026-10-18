//! Read-only, revision-pinned processing metadata. No submit, retry or authority.
use super::*;
use ai_fence_contract::workload_offer::{valid_workload_role, WorkloadOffer};
impl ApplicationClient {
    /// Read the exact profile revision previously resolved by capabilities.
    /// Freeze returned consent metadata locally; admission rechecks authority.
    pub async fn workload_offer(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        role: &str,
        profile_revision: u64,
    ) -> Result<WorkloadOffer, Error> {
        self.validate_service_credential(connection, credential)?;
        if !valid_workload_role(role)
            || profile_revision == 0
            || connection
                .profiles
                .get(role)
                .is_none_or(|p| p.revision.is_some_and(|r| r != profile_revision))
        {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .get(self.endpoint(&format!(
                "/api/v1/application-bindings/self/workloads/{role}/offer"
            ))?)
            .query(&[
                ("binding_revision", connection.binding_revision),
                ("profile_revision", profile_revision),
            ])
            .bearer_auth(&credential.token)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value: WorkloadOffer = read_json(response).await?;
        value.validate().map_err(|_| Error::InvalidResponse)?;
        if !value.matches(connection, role, profile_revision) {
            return Err(Error::BindingMismatch);
        }
        Ok(value)
    }
}
#[cfg(test)]
mod tests;
