//! Administrative, terminal binding revocation. Never supplies spending authority.
use super::*;
use ai_fence_contract::application::{BindingRevocationReceipt, RevokeBindingRequest};

impl ApplicationClient {
    /// One request only. Retain the binding identity after an unknown transport
    /// outcome, then retry it or read the durable receipt; never invent a new ID.
    pub async fn revoke_binding(
        &self,
        request: &RevokeBindingRequest,
        credential: &ManagementCredential,
    ) -> Result<BindingRevocationReceipt, Error> {
        request
            .validate()
            .map_err(|_| Error::InvalidConfiguration)?;
        let response = self
            .http
            .put(self.endpoint(&format!(
                "/api/v1/internal/application-bindings/{}/revocation",
                request.binding.binding_id
            ))?)
            .bearer_auth(&credential.0)
            .json(request)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        Self::revocation_receipt(response, &request.binding).await
    }

    pub async fn binding_revocation(
        &self,
        binding: &BindingIdentity,
        credential: &ManagementCredential,
    ) -> Result<BindingRevocationReceipt, Error> {
        RevokeBindingRequest {
            schema_version: 1,
            binding: binding.clone(),
        }
        .validate()
        .map_err(|_| Error::InvalidConfiguration)?;
        let response = self
            .http
            .get(self.endpoint(&format!(
                "/api/v1/internal/application-bindings/{}/revocation",
                binding.binding_id
            ))?)
            .bearer_auth(&credential.0)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        Self::revocation_receipt(response, binding).await
    }

    async fn revocation_receipt(
        response: reqwest::Response,
        binding: &BindingIdentity,
    ) -> Result<BindingRevocationReceipt, Error> {
        let receipt: BindingRevocationReceipt = read_json(response).await?;
        receipt.validate().map_err(|_| Error::InvalidResponse)?;
        if &receipt.binding != binding {
            return Err(Error::BindingMismatch);
        }
        Ok(receipt)
    }
}

#[cfg(test)]
#[path = "application_revocations/tests.rs"]
mod tests;
