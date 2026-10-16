//! One transport call per explicit submit or metadata reconciliation. No retry.
use super::*;
use ai_fence_contract::application_run::{
    canonical_uuid, valid_digest, RunResponse, SubmitRunRequest, MAX_RESPONSE_BYTES,
};

impl ApplicationClient {
    /// Submit one explicit attempt. On transport failure retain the dispatch key
    /// and digest, then reconcile; never resubmit with a new key automatically.
    /// A completed `not_retained` reply is final metadata, not a retry signal.
    pub async fn submit_run(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        request: &SubmitRunRequest,
    ) -> Result<RunResponse, Error> {
        self.validate_service_credential(connection, credential)?;
        request
            .validate()
            .map_err(|_| Error::InvalidConfiguration)?;
        if request.binding_revision != connection.binding_revision
            || !connection
                .profiles
                .get(&request.workload_role)
                .is_some_and(|profile| {
                    profile.id == request.profile_id
                        && profile
                            .revision
                            .is_none_or(|rev| rev == request.profile_revision)
                })
        {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .post(self.endpoint("/api/v1/application-runs")?)
            .bearer_auth(&credential.token)
            .json(request)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value: RunResponse = read_json_with_limit(response, MAX_RESPONSE_BYTES).await?;
        value.validate(true).map_err(|_| Error::InvalidResponse)?;
        if value.binding != connection.binding || !value.matches_request(request) {
            return Err(Error::RunMismatch);
        }
        Ok(value)
    }

    /// Read owned metadata even when the submit reply and run ID were lost.
    /// Historical binding revisions remain readable after a deployment update.
    pub async fn run_by_key(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        dispatch_key: &str,
        payload_digest: &str,
    ) -> Result<RunResponse, Error> {
        self.validate_service_credential(connection, credential)?;
        if !canonical_uuid(dispatch_key) || !valid_digest(payload_digest) {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .get(self.endpoint(&format!("/api/v1/application-runs/by-key/{dispatch_key}"))?)
            .query(&[("payload_digest", payload_digest)])
            .bearer_auth(&credential.token)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value = self.read_run_metadata(response, connection).await?;
        if value.dispatch_key != dispatch_key || value.payload_digest != payload_digest {
            return Err(Error::RunMismatch);
        }
        Ok(value)
    }

    pub async fn run_by_id(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        run_id: &str,
    ) -> Result<RunResponse, Error> {
        self.validate_service_credential(connection, credential)?;
        if !canonical_uuid(run_id) {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .get(self.endpoint(&format!("/api/v1/application-runs/{run_id}"))?)
            .bearer_auth(&credential.token)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value = self.read_run_metadata(response, connection).await?;
        if value.run_id != run_id {
            return Err(Error::RunMismatch);
        }
        Ok(value)
    }

    async fn read_run_metadata(
        &self,
        response: reqwest::Response,
        connection: &ManagedConnection,
    ) -> Result<RunResponse, Error> {
        if response.status() == StatusCode::CREATED {
            return Err(Error::InvalidResponse);
        }
        let value: RunResponse = read_json_with_limit(response, MAX_RESPONSE_BYTES).await?;
        value.validate(false).map_err(|_| Error::InvalidResponse)?;
        if value.binding != connection.binding
            || value.binding_revision > connection.binding_revision
        {
            return Err(Error::RunMismatch);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests;
