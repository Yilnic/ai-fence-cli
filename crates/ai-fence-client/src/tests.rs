// Tokio's test macro owns a fixture runtime; production code creates no runtime.
#![allow(clippy::disallowed_methods)]
use super::*;
use ai_fence_contract::application::{
    ApplicationBindingSpec, BindingIdentity, ProfileReference, ServiceAuthentication,
    UnavailableReason, WorkloadReadiness, BINDING_READINESS_V1,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    thread,
};

pub(super) fn fixture(origin: &str) -> ManagedConnection {
    ManagedConnection {
        schema_version: 1,
        fence_url: origin.into(),
        binding: BindingIdentity {
            binding_id: "11111111-1111-4111-8111-111111111111".into(),
            application_id: "22222222-2222-4222-8222-222222222222".into(),
            stage_id: "33333333-3333-4333-8333-333333333333".into(),
            service_name: "intelligence".into(),
        },
        binding_revision: 1,
        auth: ServiceAuthentication {
            issuer: format!("{origin}/"),
            token_endpoint: format!("{origin}/connect/token"),
            client_id: "fixture-machine".into(),
            audience: "fixture-service".into(),
            scope: "app.fixture.ai".into(),
            client_secret_ref: "secret:identity.fixture.client.ai.client_secret".into(),
        },
        profiles: BTreeMap::from([(
            "review".into(),
            ProfileReference {
                id: "operator-profile".into(),
                revision: None,
            },
        )]),
    }
}
fn capabilities(connection: &ManagedConnection) -> ApplicationCapabilities {
    ApplicationCapabilities {
        schema_version: 1,
        binding: connection.binding.clone(),
        binding_revision: 1,
        enabled: true,
        enforced_capabilities: vec![BINDING_READINESS_V1.into()],
        workloads: BTreeMap::from([(
            "review".into(),
            WorkloadReadiness {
                profile_id: "operator-profile".into(),
                profile_revision: None,
                ready: false,
                reason: Some(UnavailableReason::RunExecutionNotImplemented),
                enforced_capabilities: vec![],
            },
        )]),
    }
}
pub(super) fn response(status: u16, body: &str) -> String {
    format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
}
pub(super) fn server(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
    server_with(|_| responses)
}

fn server_with(
    make: impl FnOnce(&str) -> Vec<String>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let responses = make(&origin);
    let handle = thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0u8; 1024];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 128 * 1024);
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..header_end]);
                        let len = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= header_end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = stream.write_all(response.as_bytes());
                String::from_utf8(bytes).unwrap()
            })
            .collect()
    });
    (origin, handle)
}
fn token() -> String {
    serde_json::to_string(
        &json!({"access_token":"synthetic-service-token","token_type":"Bearer","expires_in":60}),
    )
    .unwrap()
}

#[tokio::test]
async fn actual_oauth_then_readiness_keeps_provisioning_separate_from_inference() {
    let (origin, handle) = server_with(|origin| {
        vec![
            response(200, &token()),
            response(
                200,
                &serde_json::to_string(&capabilities(&fixture(origin))).unwrap(),
            ),
        ]
    });
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let credential = client
        .authenticate(
            &connection,
            &ClientSecret::new("synthetic-client-secret".into()).unwrap(),
        )
        .await
        .unwrap();
    let ready = client.capabilities(&connection, &credential).await.unwrap();
    assert!(!ready.supports_workload("review", &["structured_document_input_v1"]));
    let requests = handle.join().unwrap();
    assert!(requests[0].starts_with("POST /connect/token "));
    assert!(requests[0].contains("grant_type=client_credentials"));
    assert!(requests[1].starts_with("GET /api/v1/application-bindings/self/capabilities "));
    assert!(requests[1].contains("Bearer synthetic-service-token"));
    assert!(!requests[1].contains("synthetic-client-secret"));
}

#[tokio::test]
async fn redirected_credentials_are_never_forwarded_and_errors_do_not_echo_bodies() {
    let (origin,handle)=server(vec!["HTTP/1.1 302 fixture\r\nLocation: http://127.0.0.1:1/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()]);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let result = client
        .authenticate(
            &fixture(&origin),
            &ClientSecret::new("synthetic-client-secret".into()).unwrap(),
        )
        .await;
    assert!(matches!(result, Err(Error::Http(302))));
    handle.join().unwrap();
    let (origin, handle) = server(vec![response(500, "synthetic-secret-and-private-content")]);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let result = client
        .authenticate(
            &fixture(&origin),
            &ClientSecret::new("synthetic-client-secret".into()).unwrap(),
        )
        .await;
    let error = match result {
        Err(error) => error,
        _ => panic!("error expected"),
    };
    assert_eq!(error, Error::Http(500));
    assert!(!error.to_string().contains("synthetic-secret"));
    handle.join().unwrap();
}

#[tokio::test]
async fn response_limits_apply_without_a_content_length_header() {
    let body = "x".repeat(MAX_RESPONSE_BYTES + 1);
    let (origin, handle) = server(vec![format!(
        "HTTP/1.1 200 fixture\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
    )]);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let result = client
        .authenticate(
            &fixture(&origin),
            &ClientSecret::new("synthetic-client-secret".into()).unwrap(),
        )
        .await;
    assert!(matches!(result, Err(Error::ResponseTooLarge)));
    handle.join().unwrap();
}

#[tokio::test]
async fn reconciliation_checks_the_returned_binding_and_preserves_wire_request() {
    let connection = fixture("http://127.0.0.1:1");
    let spec = ApplicationBindingSpec {
        application_id: connection.binding.application_id.clone(),
        stage_id: connection.binding.stage_id.clone(),
        service_name: "intelligence".into(),
        issuer: connection.auth.issuer,
        client_id: connection.auth.client_id,
        audience: connection.auth.audience,
        scope: connection.auth.scope,
        consumer_origin: "https://app.example.test".into(),
        profiles: BTreeMap::from([("review".into(), "operator-profile".into())]),
        enabled: true,
    };
    let request = ReconcileBindingRequest {
        schema_version: 1,
        expected_revision: 0,
        spec,
    };
    let result = ReconcileBindingResponse {
        binding: ApplicationBinding {
            schema_version: 1,
            binding_id: connection.binding.binding_id.clone(),
            revision: 1,
            spec: request.spec.clone(),
        },
        changed: true,
    };
    let (origin, handle) = server(vec![response(
        201,
        &serde_json::to_string(&result).unwrap(),
    )]);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    client
        .reconcile_binding(
            &connection.binding.binding_id,
            &request,
            &ManagementCredential::new("synthetic-management-token".into()).unwrap(),
        )
        .await
        .unwrap();
    let wire = handle.join().unwrap().remove(0);
    assert!(wire.starts_with("PUT /api/v1/internal/application-bindings/"));
    assert!(wire.contains("\"expected_revision\":0"));
    let mut foreign = result;
    foreign.binding.spec.stage_id = "other-stage".into();
    let (origin, handle) = server(vec![response(
        200,
        &serde_json::to_string(&foreign).unwrap(),
    )]);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    assert_eq!(
        client
            .reconcile_binding(
                &connection.binding.binding_id,
                &request,
                &ManagementCredential::new("synthetic-management-token".into()).unwrap()
            )
            .await
            .unwrap_err(),
        Error::BindingMismatch
    );
    handle.join().unwrap();
}

#[test]
fn configuration_rejects_credential_urls_remote_http_and_untrusted_token_origins() {
    for url in [
        "http://remote.example.test",
        "https://user:secret@fence.example.test",
        "https://fence.example.test/path",
        "https://fence.example.test?token=secret",
        "https://fence.example.test/#fragment",
    ] {
        assert!(ApplicationClient::new(url, Duration::from_secs(5)).is_err());
    }
    let client = ApplicationClient::new("http://127.0.0.1:1", Duration::from_secs(5)).unwrap();
    let mut connection = fixture("http://127.0.0.1:1");
    connection.auth.token_endpoint = "https://foreign.example.test/token".into();
    assert_eq!(
        client.validate_connection(&connection),
        Err(Error::InvalidConfiguration)
    );
}
