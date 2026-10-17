#![allow(clippy::disallowed_methods)]
use super::*;
use ai_fence_contract::{
    application::{BindingIdentity, ProfileReference, ServiceAuthentication},
    application_run::*,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    thread,
};

#[path = "audio_tests.rs"]
mod audio;
#[path = "v2_tests.rs"]
mod v2;

fn fixture(origin: &str) -> ManagedConnection {
    ManagedConnection {
        schema_version: 1,
        fence_url: origin.into(),
        binding: BindingIdentity {
            binding_id: "11111111-1111-4111-8111-111111111111".into(),
            application_id: "22222222-2222-4222-8222-222222222222".into(),
            stage_id: "33333333-3333-4333-8333-333333333333".into(),
            service_name: "ai".into(),
        },
        binding_revision: 2,
        auth: ServiceAuthentication {
            issuer: origin.into(),
            token_endpoint: format!("{origin}/token"),
            client_id: "fixture-client".into(),
            audience: "fixture-audience".into(),
            scope: "app.fixture.ai".into(),
            client_secret_ref: "secret:fixture.identity".into(),
        },
        profiles: BTreeMap::from([(
            "learning_practice_text".into(),
            ProfileReference {
                id: "practice".into(),
                revision: Some(3),
            },
        )]),
    }
}
fn request() -> SubmitRunRequest {
    let mut request = SubmitRunRequest {
        schema_version: 1,
        binding_revision: 2,
        workload_role: "learning_practice_text".into(),
        profile_id: "practice".into(),
        profile_revision: 3,
        operation_id: "44444444-4444-4444-8444-444444444444".into(),
        attempt_number: 1,
        dispatch_key: "55555555-5555-4555-8555-555555555555".into(),
        payload_digest: String::new(),
        funding: Funding::ApplicationApi,
        retention: Retention::MetadataOnly,
        input_kind: None,
        receipt_version: None,
        input: StructuredTextInput {
            instructions: "private instructions".into(),
            prompt: "private learner prompt".into(),
            schema_name: "practice".into(),
            json_schema: json!({"type":"object"}),
            max_output_tokens: 50,
        }
        .into(),
    };
    request.payload_digest = request.canonical_digest().unwrap();
    request
}
fn completed(connection: &ManagedConnection) -> RunResponse {
    let request = request();
    let receipt = RunReceipt {
        input_audio_ms: None,
        schema_version: 1,
        run_id: "66666666-6666-4666-8666-666666666666".into(),
        attempt_id: "77777777-7777-4777-8777-777777777777".into(),
        provider_call_id: "88888888-8888-4888-8888-888888888888".into(),
        binding: connection.binding.clone(),
        binding_revision: 2,
        profile_id: request.profile_id.clone(),
        profile_revision: 3,
        policy_revision: 4,
        selected_model: "fixture-model".into(),
        usage_state: UsageState::Unknown,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        reasoning_tokens: None,
        total_tokens: None,
        cache_write_tokens: None,
        settlement_state: None,
        price_state: PriceState::Unknown,
        usd_amount: None,
        price_snapshot_ref: None,
    };
    RunResponse {
        schema_version: 1,
        run_id: receipt.run_id.clone(),
        attempt_id: receipt.attempt_id.clone(),
        binding: connection.binding.clone(),
        binding_revision: 2,
        operation_id: request.operation_id,
        attempt_number: 1,
        dispatch_key: request.dispatch_key,
        payload_digest: request.payload_digest,
        workload_role: request.workload_role,
        profile_id: request.profile_id,
        profile_revision: 3,
        policy_revision: 4,
        input_kind: None,
        receipt_version: None,
        status: RunStatus::Completed,
        reason: None,
        receipt: Some(receipt),
        result_delivery: ResultDelivery::Inline,
        result: Some(json!({"answer":"private generated result"})),
    }
}
fn credential(connection: &ManagedConnection) -> ServiceCredential {
    ServiceCredential {
        token: "synthetic-service-token".into(),
        expires: Instant::now() + Duration::from_secs(60),
        identity: CredentialIdentity::capture(
            &Url::parse(&connection.fence_url).unwrap(),
            connection,
        ),
    }
}

#[tokio::test]
async fn authenticated_credential_cannot_cross_identity_or_origin_before_content_transport() {
    let (origin, handle) = server(|origin| {
        vec![
        response(200, &json!({"access_token":"synthetic-service-token","token_type":"Bearer","expires_in":60}).to_string()),
        wire(&completed(&fixture(origin)).without_result()),
    ]
    });
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(2)).unwrap();
    let credential = client
        .authenticate(
            &connection,
            &ClientSecret::new("fixture-client-secret".into()).unwrap(),
        )
        .await
        .unwrap();
    let request = request();
    let mut alternatives = Vec::new();
    for field in [
        "binding_id",
        "application_id",
        "stage_id",
        "service",
        "issuer",
        "client",
        "audience",
        "scope",
    ] {
        let mut other = connection.clone();
        match field {
            "binding_id" => {
                other.binding.binding_id = "99999999-9999-4999-8999-999999999999".into()
            }
            "application_id" => {
                other.binding.application_id = "99999999-9999-4999-8999-999999999999".into()
            }
            "stage_id" => other.binding.stage_id = "99999999-9999-4999-8999-999999999999".into(),
            "service" => other.binding.service_name = "other-service".into(),
            "issuer" => other.auth.issuer = format!("{origin}/other-realm"),
            "client" => other.auth.client_id = "other-client".into(),
            "audience" => other.auth.audience = "other-audience".into(),
            "scope" => other.auth.scope = "app.other.ai".into(),
            _ => unreachable!(),
        }
        alternatives.push(other);
    }
    for other in alternatives {
        assert_eq!(
            client.capabilities(&other, &credential).await.unwrap_err(),
            Error::BindingMismatch
        );
        assert_eq!(
            client
                .submit_run(&other, &credential, &request)
                .await
                .unwrap_err(),
            Error::BindingMismatch
        );
        assert_eq!(
            client
                .run_by_key(
                    &other,
                    &credential,
                    &request.dispatch_key,
                    &request.payload_digest
                )
                .await
                .unwrap_err(),
            Error::BindingMismatch
        );
        assert_eq!(
            client
                .run_by_id(&other, &credential, &completed(&connection).run_id)
                .await
                .unwrap_err(),
            Error::BindingMismatch
        );
    }
    // The other origin remains listening: a wrong request cannot disappear into
    // a closed port and masquerade as a successful pre-transport refusal.
    let other_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    other_listener.set_nonblocking(true).unwrap();
    let other_origin = format!("http://{}", other_listener.local_addr().unwrap());
    let other_client = ApplicationClient::new(&other_origin, Duration::from_secs(1)).unwrap();
    let other = fixture(&other_origin);
    assert_eq!(
        other_client
            .capabilities(&other, &credential)
            .await
            .unwrap_err(),
        Error::BindingMismatch
    );
    assert_eq!(
        other_client
            .submit_run(&other, &credential, &request)
            .await
            .unwrap_err(),
        Error::BindingMismatch
    );
    assert_eq!(
        other_client
            .run_by_key(
                &other,
                &credential,
                &request.dispatch_key,
                &request.payload_digest
            )
            .await
            .unwrap_err(),
        Error::BindingMismatch
    );
    assert_eq!(
        other_client
            .run_by_id(&other, &credential, &completed(&connection).run_id)
            .await
            .unwrap_err(),
        Error::BindingMismatch
    );
    assert_eq!(
        other_listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    // Revisions and profiles are not token identities; historical paid receipts
    // remain accessible after a legitimate deployment/profile update.
    let mut updated = connection;
    updated.binding_revision += 1;
    updated.profiles.get_mut(&request.workload_role).unwrap().id = "next-operator-profile".into();
    let metadata = client
        .run_by_key(
            &updated,
            &credential,
            &request.dispatch_key,
            &request.payload_digest,
        )
        .await
        .unwrap();
    assert_eq!(metadata.result_delivery, ResultDelivery::NotRetained);
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /token "));
    assert!(requests[1].starts_with("GET /api/v1/application-runs/by-key/"));
    assert!(requests
        .iter()
        .all(|wire| !wire.contains("private learning prompt")));
}
fn wire(value: &RunResponse) -> String {
    response(200, &serde_json::to_string(value).unwrap())
}
fn response(status: u16, body: &str) -> String {
    format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
}
fn server(make: impl FnOnce(&str) -> Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let replies = make(&origin);
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        replies
            .into_iter()
            .map(|reply| {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("missing fixture request: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < MAX_REQUEST_BYTES + 16 * 1024);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = stream.write_all(reply.as_bytes());
                String::from_utf8(bytes).unwrap()
            })
            .collect()
    });
    (origin, handle)
}

#[tokio::test]
async fn lost_submit_reply_is_reconciled_by_key_without_another_dispatch_or_result_replay() {
    let (origin, handle) = server(|origin| {
        vec![
            String::new(),
            wire(&completed(&fixture(origin)).without_result()),
        ]
    });
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let request = request();
    let credential = credential(&fixture(&origin));
    assert!(matches!(
        client.submit_run(&connection, &credential, &request).await,
        Err(Error::Transport)
    ));
    let metadata = client
        .run_by_key(
            &connection,
            &credential,
            &request.dispatch_key,
            &request.payload_digest,
        )
        .await
        .unwrap();
    assert_eq!(metadata.result_delivery, ResultDelivery::NotRetained);
    assert!(metadata.result.is_none());
    assert_eq!(metadata.receipt.as_ref().unwrap().usd_amount, None);
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /api/v1/application-runs "));
    assert!(requests[1].starts_with(&format!(
        "GET /api/v1/application-runs/by-key/{}?payload_digest={} ",
        request.dispatch_key, request.payload_digest
    )));
    assert!(!requests[1].contains("private learner prompt"));
    assert!(!requests[1].contains("private instructions"));
    assert!(requests
        .iter()
        .all(|r| r.contains("Bearer synthetic-service-token")));
}

#[tokio::test]
async fn explicit_submit_and_duplicate_are_one_call_each_and_preserve_the_typed_request() {
    let (origin, handle) = server(|origin| {
        let result = completed(&fixture(origin));
        vec![wire(&result), wire(&result.without_result())]
    });
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let request = request();
    let credential = credential(&fixture(&origin));
    let first = client
        .submit_run(&connection, &credential, &request)
        .await
        .unwrap();
    assert_eq!(first.result_delivery, ResultDelivery::Inline);
    assert_eq!(
        first.result,
        Some(json!({"answer":"private generated result"}))
    );
    let duplicate = client
        .submit_run(&connection, &credential, &request)
        .await
        .unwrap();
    assert_eq!(duplicate.run_id, first.run_id);
    assert_eq!(duplicate.result_delivery, ResultDelivery::NotRetained);
    for wire in handle.join().unwrap() {
        let body = wire.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(parse_request(body.as_bytes()).unwrap(), request);
    }
}

#[tokio::test]
async fn returned_binding_key_digest_operation_and_profile_must_match_the_attempt() {
    for field in [
        "binding",
        "binding_revision",
        "dispatch_key",
        "payload_digest",
        "operation_id",
        "profile_id",
        "profile_revision",
        "attempt_number",
    ] {
        let (origin, handle) = server(|origin| {
            let mut value =
                serde_json::to_value(completed(&fixture(origin)).without_result()).unwrap();
            match field {
                "binding" => {
                    value["binding"]["stage_id"] = json!("99999999-9999-4999-8999-999999999999");
                    value["receipt"]["binding"] = value["binding"].clone();
                }
                "binding_revision" | "profile_revision" => {
                    value[field] = json!(5);
                    value["receipt"][field] = json!(5);
                }
                "attempt_number" => value[field] = json!(2),
                "payload_digest" => value[field] = json!("0".repeat(64)),
                "profile_id" => {
                    value[field] = json!("foreign-profile");
                    value["receipt"][field] = json!("foreign-profile");
                }
                _ => value[field] = json!("99999999-9999-4999-8999-999999999999"),
            }
            vec![response(200, &value.to_string())]
        });
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        assert_eq!(
            client
                .submit_run(
                    &fixture(&origin),
                    &credential(&fixture(&origin)),
                    &request()
                )
                .await
                .unwrap_err(),
            Error::RunMismatch,
            "{field}"
        );
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn reconciliation_rejects_inline_or_foreign_metadata_and_allows_historical_revisions() {
    for variant in ["inline", "key", "digest", "run_id", "historical"] {
        let (origin, handle) = server(|origin| {
            let mut result = completed(&fixture(origin));
            if variant != "inline" {
                result = result.without_result();
            }
            match variant {
                "key" => result.dispatch_key = "99999999-9999-4999-8999-999999999999".into(),
                "digest" => result.payload_digest = "0".repeat(64),
                "run_id" => {
                    result.run_id = "99999999-9999-4999-8999-999999999999".into();
                    result.receipt.as_mut().unwrap().run_id = result.run_id.clone();
                }
                _ => {}
            }
            vec![wire(&result)]
        });
        let mut connection = fixture(&origin);
        if variant == "historical" {
            connection.binding_revision = 3;
        }
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        let request = request();
        let result = if variant == "run_id" {
            client
                .run_by_id(
                    &connection,
                    &credential(&fixture(&origin)),
                    "66666666-6666-4666-8666-666666666666",
                )
                .await
        } else {
            client
                .run_by_key(
                    &connection,
                    &credential(&fixture(&origin)),
                    &request.dispatch_key,
                    &request.payload_digest,
                )
                .await
        };
        if variant == "historical" {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err(), "{variant}");
        }
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn bounded_run_bodies_redirects_and_error_content_are_not_retried_or_exposed() {
    for reply in [
        format!("HTTP/1.1 200 fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",MAX_RESPONSE_BYTES+1),
        format!("HTTP/1.1 200 fixture\r\nConnection: close\r\n\r\n{}","x".repeat(MAX_RESPONSE_BYTES+1)),
        "HTTP/1.1 302 fixture\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        response(500,"private-provider-body synthetic-service-token"),
    ] {
        let (origin,handle)=server(|_|vec![reply]);
        let client=ApplicationClient::new(&origin,Duration::from_secs(5)).unwrap();
        let error=client.submit_run(&fixture(&origin),&credential(&fixture(&origin)),&request()).await.unwrap_err();
        assert!(matches!(error,Error::ResponseTooLarge|Error::Http(302|500)));
        assert!(!format!("{error:?} {error}").contains("private-provider-body"));
        assert!(!error.to_string().contains("synthetic-service-token"));
        assert_eq!(handle.join().unwrap().len(),1);
    }
}

#[tokio::test]
async fn malformed_requests_and_expired_credentials_are_rejected_before_network() {
    let origin = String::from("http://127.0.0.1:1");
    let connection = fixture("http://127.0.0.1:1");
    let client = ApplicationClient::new(&connection.fence_url, Duration::from_secs(1)).unwrap();
    let mut changed = request();
    changed.input.as_structured_mut().unwrap().prompt.push('!');
    assert_eq!(
        client
            .submit_run(&connection, &credential(&fixture(&origin)), &changed)
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let mut changed = request();
    changed.binding_revision += 1;
    changed.payload_digest = changed.canonical_digest().unwrap();
    assert_eq!(
        client
            .submit_run(&connection, &credential(&fixture(&origin)), &changed)
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let expired = ServiceCredential {
        token: "private-expired-token".into(),
        expires: Instant::now() - Duration::from_secs(1),
        identity: CredentialIdentity::capture(
            &Url::parse(&connection.fence_url).unwrap(),
            &connection,
        ),
    };
    assert_eq!(
        client
            .submit_run(&connection, &expired, &request())
            .await
            .unwrap_err(),
        Error::ExpiredCredential
    );
    assert_eq!(
        client
            .run_by_key(
                &connection,
                &credential(&fixture(&origin)),
                "../escape",
                "bad"
            )
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
}

#[path = "v1_recovery_tests.rs"]
mod v1_recovery;

#[path = "document_tests.rs"]
mod documents;
