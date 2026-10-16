// Tokio's test macro owns the fixture runtime; production code creates none.
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::tests::{fixture, response, server};
use serde_json::json;

fn receipt(binding: &BindingIdentity) -> BindingRevocationReceipt {
    serde_json::from_value(json!({
        "schema_version":1,"binding":binding,
        "revocation_id":"99999999-9999-4999-8999-999999999999",
        "revoked_at":"2026-10-15T12:00:00Z"
    }))
    .unwrap()
}

#[tokio::test]
async fn unknown_revocation_retries_same_identity_and_reads_same_durable_receipt() {
    let binding = fixture("http://127.0.0.1:1").binding;
    let expected = receipt(&binding);
    let wire = serde_json::to_string(&expected).unwrap();
    let (origin, handle) = server(vec![
        String::new(),
        response(200, &wire),
        response(200, &wire),
    ]);
    let client = ApplicationClient::new(&origin, Duration::from_secs(2)).unwrap();
    let management = ManagementCredential::new("synthetic-admin-secret".into()).unwrap();
    let request = RevokeBindingRequest {
        schema_version: 1,
        binding: binding.clone(),
    };
    assert_eq!(
        client
            .revoke_binding(&request, &management)
            .await
            .unwrap_err(),
        Error::Transport
    );
    let retried = client.revoke_binding(&request, &management).await.unwrap();
    let read = client
        .binding_revocation(&binding, &management)
        .await
        .unwrap();
    assert_eq!(retried, expected);
    assert_eq!(read, expected);
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 3);
    let path = format!(
        "/api/v1/internal/application-bindings/{}/revocation",
        binding.binding_id
    );
    assert!(requests[0].starts_with(&format!("PUT {path} ")));
    assert!(requests[1].starts_with(&format!("PUT {path} ")));
    assert!(requests[2].starts_with(&format!("GET {path} ")));
    assert_eq!(
        requests[0].split_once("\r\n\r\n").unwrap().1,
        requests[1].split_once("\r\n\r\n").unwrap().1
    );
    assert!(!requests[0]
        .split_once("\r\n\r\n")
        .unwrap()
        .1
        .contains("synthetic-admin-secret"));
}

#[tokio::test]
async fn revocation_replies_cannot_change_owner_schema_or_receipt_identity() {
    let binding = fixture("http://127.0.0.1:1").binding;
    let expected = receipt(&binding);
    for (field, value, error) in [
        (
            "binding",
            json!({"binding_id":binding.binding_id,"application_id":binding.application_id,
            "stage_id":"99999999-9999-4999-8999-999999999999","service_name":binding.service_name}),
            Error::BindingMismatch,
        ),
        ("schema_version", json!(2), Error::InvalidResponse),
        (
            "revocation_id",
            json!("00000000-0000-0000-0000-000000000000"),
            Error::InvalidResponse,
        ),
        ("extra", json!("untrusted response"), Error::InvalidResponse),
    ] {
        let mut body = serde_json::to_value(&expected).unwrap();
        body[field] = value;
        let wire = body.to_string();
        let (origin, handle) = server(vec![response(200, &wire), response(200, &wire)]);
        let client = ApplicationClient::new(&origin, Duration::from_secs(2)).unwrap();
        let management = ManagementCredential::new("synthetic-admin-secret".into()).unwrap();
        assert_eq!(
            client
                .revoke_binding(
                    &RevokeBindingRequest {
                        schema_version: 1,
                        binding: binding.clone()
                    },
                    &management
                )
                .await
                .unwrap_err(),
            error
        );
        assert_eq!(
            client
                .binding_revocation(&binding, &management)
                .await
                .unwrap_err(),
            error
        );
        assert_eq!(handle.join().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn revocation_transport_is_bounded_redacted_and_never_follows_redirects() {
    let binding = fixture("http://127.0.0.1:1").binding;
    for reply in [
        response(500, "private-provider-error synthetic-admin-secret"),
        "HTTP/1.1 302 fixture\r\nLocation: http://127.0.0.1:9/credential-trap\r\nContent-Length: 0\r\n\r\n".into(),
        response(200, &"x".repeat(MAX_RESPONSE_BYTES + 1)),
    ] {
        let (origin, handle) = server(vec![reply]);
        let client = ApplicationClient::new(&origin, Duration::from_secs(2)).unwrap();
        let error = client.revoke_binding(&RevokeBindingRequest { schema_version: 1, binding: binding.clone() },
            &ManagementCredential::new("synthetic-admin-secret".into()).unwrap()).await.unwrap_err();
        assert!(matches!(error, Error::Http(302 | 500) | Error::ResponseTooLarge));
        assert!(!format!("{error:?} {error}").contains("private-provider-error"));
        assert!(!format!("{error:?} {error}").contains("synthetic-admin-secret"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}
