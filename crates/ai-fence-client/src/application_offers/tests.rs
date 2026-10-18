#![allow(clippy::disallowed_methods)]
use super::*;
use crate::tests::{fixture, response, server};
use serde_json::{json, Value};
fn offer() -> Value {
    json!({"schema_version":1,"binding":fixture("http://127.0.0.1:1").binding,"binding_revision":1,"workload_role":"review","profile_id":"operator-profile","profile_revision":7,
        "input_kind":"structured_document","provider_origin":"https://provider.example.test","selected_model":"original-document-model",
        "data_policy":{"url":"https://provider.example.test/privacy","revision":"policy-3","source":"operator_configured"},"gateway_retention":"metadata_only",
        "max_request_bytes":8192,"max_input_tokens":65536,"max_output_tokens":4096,"documents":{"max_input_bytes":32768,"allowed_media_types":["application/pdf","image/png"]},"speech":null,
        "pricing":{"unit":"tokens","snapshot":{"schema_version":2,"input":2,"cached_input":1,"cache_write":3,"output":4,"tiers":[]}},"maximum_estimated_call_usd":0.4,
        "enforced_capabilities":["structured_document_input_v1","full_run_receipts_v2","metadata_only_retention_v1","application_workload_offer_v1"]})
}
fn credential(connection: &ManagedConnection) -> ServiceCredential {
    ServiceCredential {
        token: "synthetic-offer-machine-token".into(),
        expires: Instant::now() + Duration::from_secs(60),
        identity: CredentialIdentity::capture(
            &Url::parse(&connection.fence_url).unwrap(),
            connection,
        ),
    }
}
#[tokio::test]
async fn workload_offer_is_one_pinned_authenticated_get_with_original_metadata() {
    let expected: WorkloadOffer = serde_json::from_value(offer()).unwrap();
    let (origin, handle) = server(vec![response(200, &offer().to_string())]);
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let actual = client
        .workload_offer(&connection, &credential(&connection), "review", 7)
        .await
        .unwrap();
    assert_eq!(actual, expected);
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /api/v1/application-bindings/self/workloads/review/offer?binding_revision=1&profile_revision=7 HTTP/1.1"));
    assert!(requests[0].contains("Bearer synthetic-offer-machine-token"));
    assert!(requests[0].ends_with("\r\n\r\n"));
    assert!(!requests[0].contains("client_secret"));
}
#[tokio::test]
async fn workload_offer_refuses_foreign_or_invalid_metadata_without_retries() {
    for case in 0..9 {
        let mut body = offer();
        match case {
            0 => body["binding"]["application_id"] = "foreign".into(),
            1 => body["binding_revision"] = 2.into(),
            2 => body["profile_revision"] = 8.into(),
            3 => body["profile_id"] = "foreign".into(),
            4 => body["workload_role"] = "other".into(),
            5 => body["provider_origin"] = "https://provider.example.test/private".into(),
            6 => body["pricing"]["snapshot"]
                .as_object_mut()
                .unwrap()
                .remove("cache_write")
                .map(|_| ())
                .unwrap(),
            7 => {
                body["speech"] = json!({"limits":{"max_input_bytes":32768,"max_input_duration_ms":1000},"segment_timestamps":true})
            }
            _ => body["credential"] = "REFLECTED_PRIVATE_SENTINEL".into(),
        };
        let (origin, handle) = server(vec![response(200, &body.to_string())]);
        let connection = fixture(&origin);
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        let error = client
            .workload_offer(&connection, &credential(&connection), "review", 7)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidResponse | Error::BindingMismatch),
            "{case}: {error}"
        );
        assert!(!error.to_string().contains("SENTINEL"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn workload_offer_never_retries_redirect_denial_unavailable_or_oversized_response() {
    for (wire, expected) in [
        (
            response(302, "REFLECTED_PRIVATE_SENTINEL"),
            Error::Http(302),
        ),
        (
            response(401, "REFLECTED_PRIVATE_SENTINEL"),
            Error::Http(401),
        ),
        (
            response(503, "REFLECTED_PRIVATE_SENTINEL"),
            Error::Http(503),
        ),
        (response(200, &"x".repeat(65537)), Error::ResponseTooLarge),
    ] {
        let (origin, handle) = server(vec![wire]);
        let connection = fixture(&origin);
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        let error = client
            .workload_offer(&connection, &credential(&connection), "review", 7)
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!error.to_string().contains("SENTINEL"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn workload_offer_invalid_selection_and_foreign_credential_send_nothing() {
    let connection = fixture("http://127.0.0.1:1");
    let client = ApplicationClient::new(&connection.fence_url, Duration::from_secs(1)).unwrap();
    for (role, revision) in [("../other", 7), ("missing", 7), ("review", 0)] {
        assert_eq!(
            client
                .workload_offer(&connection, &credential(&connection), role, revision)
                .await
                .unwrap_err(),
            Error::InvalidConfiguration
        );
    }
    let mut pinned = connection.clone();
    pinned.profiles.get_mut("review").unwrap().revision = Some(6);
    assert_eq!(
        client
            .workload_offer(&pinned, &credential(&pinned), "review", 7)
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let mut foreign = connection.clone();
    foreign.binding.stage_id = "foreign".into();
    assert_eq!(
        client
            .workload_offer(&connection, &credential(&foreign), "review", 7)
            .await
            .unwrap_err(),
        Error::BindingMismatch
    );
    let mut expired = credential(&connection);
    expired.expires = Instant::now();
    assert_eq!(
        client
            .workload_offer(&connection, &expired, "review", 7)
            .await
            .unwrap_err(),
        Error::ExpiredCredential
    );
}
