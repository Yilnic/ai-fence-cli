//! Repeatable creation fencing with the original key/digest, never a new attempt.
use super::*;
use ai_fence_contract::{
    application_cancellation::{
        CancelRunRequest, RunCancellationReceipt, MAX_CANCELLATION_RESPONSE_BYTES,
    },
    application_run::canonical_uuid,
};

impl ApplicationClient {
    /// A transport error leaves cancellation uncertain. The caller may explicitly
    /// repeat this exact key/digest to recover the original committed proof.
    /// NotCancelled is only an observation; it does not authorize releasing funds.
    pub async fn cancel_run_by_key(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        dispatch_key: &str,
        payload_digest: &str,
    ) -> Result<RunCancellationReceipt, Error> {
        self.validate_service_credential(connection, credential)?;
        let request = CancelRunRequest {
            schema_version: 1,
            payload_digest: payload_digest.into(),
        };
        request
            .validate()
            .map_err(|_| Error::InvalidConfiguration)?;
        if !canonical_uuid(dispatch_key) {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .post(self.endpoint(&format!(
                "/api/v1/application-runs/by-key/{dispatch_key}/cancel"
            ))?)
            .bearer_auth(&credential.token)
            .json(&request)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status().is_success() && response.status() != StatusCode::OK {
            return Err(Error::InvalidResponse);
        }
        let receipt: RunCancellationReceipt =
            read_json_with_limit(response, MAX_CANCELLATION_RESPONSE_BYTES).await?;
        receipt.validate().map_err(|_| Error::InvalidResponse)?;
        if !receipt.matches_attempt(&connection.binding, dispatch_key, payload_digest)
            || receipt.binding_revision > connection.binding_revision
        {
            return Err(Error::RunMismatch);
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests;
