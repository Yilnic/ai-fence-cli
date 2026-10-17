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
        self.validate_run_request(connection, credential, request)?;
        if request.input.as_audio().is_some() || request.input.as_document().is_some() {
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
        self.read_submission(response, connection, request).await
    }

    /// Submit canonical audio once. The caller persists the same request key
    /// and digest for metadata recovery; neither audio nor result is replayed.
    pub async fn submit_transcription(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        request: &SubmitRunRequest,
        audio: ai_fence_contract::audio_transcription::PreparedAudio,
    ) -> Result<RunResponse, Error> {
        self.validate_run_request(connection, credential, request)?;
        let input = request
            .input
            .as_audio()
            .ok_or(Error::InvalidConfiguration)?;
        if !audio.matches(input) {
            return Err(Error::InvalidConfiguration);
        }
        let metadata = serde_json::to_vec(request).map_err(|_| Error::InvalidConfiguration)?;
        let form = reqwest::multipart::Form::new()
            .part(
                "metadata",
                reqwest::multipart::Part::bytes(metadata)
                    .mime_str("application/json")
                    .map_err(|_| Error::InvalidConfiguration)?,
            )
            .part(
                "file",
                reqwest::multipart::Part::bytes(audio.into_bytes())
                    .file_name("audio.wav")
                    .mime_str("audio/wav")
                    .map_err(|_| Error::InvalidConfiguration)?,
            );
        let response = self
            .http
            .post(self.endpoint("/api/v1/application-runs/transcriptions")?)
            .bearer_auth(&credential.token)
            .multipart(form)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        self.read_submission(response, connection, request).await
    }

    /// Send original document bytes once; use the original key/digest for GET
    /// recovery. No provider file storage, source filename, fallback or replay.
    pub async fn submit_document(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        request: &SubmitRunRequest,
        document: ai_fence_contract::document_input::PreparedDocument,
    ) -> Result<RunResponse, Error> {
        self.validate_run_request(connection, credential, request)?;
        let input = request
            .input
            .as_document()
            .ok_or(Error::InvalidConfiguration)?;
        if !document.matches(&input.source) {
            return Err(Error::InvalidConfiguration);
        }
        let media = input.source.media_type;
        let metadata = serde_json::to_vec(request).map_err(|_| Error::InvalidConfiguration)?;
        let form = reqwest::multipart::Form::new()
            .part(
                "metadata",
                reqwest::multipart::Part::bytes(metadata)
                    .mime_str("application/json")
                    .map_err(|_| Error::InvalidConfiguration)?,
            )
            .part(
                "file",
                reqwest::multipart::Part::bytes(document.into_bytes())
                    .file_name(media.transport_filename())
                    .mime_str(media.mime_type())
                    .map_err(|_| Error::InvalidConfiguration)?,
            );
        let response = self
            .http
            .post(self.endpoint("/api/v1/application-runs/documents")?)
            .bearer_auth(&credential.token)
            .multipart(form)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        self.read_submission(response, connection, request).await
    }

    fn validate_run_request(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
        request: &SubmitRunRequest,
    ) -> Result<(), Error> {
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
        Ok(())
    }

    async fn read_submission(
        &self,
        response: reqwest::Response,
        connection: &ManagedConnection,
        request: &SubmitRunRequest,
    ) -> Result<RunResponse, Error> {
        let value: RunResponse = read_json_with_limit(response, MAX_RESPONSE_BYTES).await?;
        value.validate(true).map_err(|_| Error::InvalidResponse)?;
        if value.binding != connection.binding || !value.matches_request(request) {
            return Err(Error::RunMismatch);
        }
        if let (Some(input), Some(result)) = (request.input.as_native(), &value.result) {
            let output: ai_fence_contract::application_native::NativeTurnOutput =
                serde_json::from_value(result.clone()).map_err(|_| Error::InvalidResponse)?;
            output
                .validate_for(input)
                .map_err(|_| Error::InvalidResponse)?;
        }
        if let (Some(input), Some(result)) = (request.input.as_audio(), &value.result) {
            let output: ai_fence_contract::audio_transcription::TranscriptionOutput =
                serde_json::from_value(result.clone()).map_err(|_| Error::InvalidResponse)?;
            output
                .validate_for(input.source_duration_ms, input.segment_timestamps)
                .map_err(|_| Error::InvalidResponse)?;
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
