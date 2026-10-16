#![allow(clippy::disallowed_methods)]
use super::*;
use ai_fence_contract::{
    application::{BindingIdentity, ProfileReference, ServiceAuthentication},
    application_cancellation::{CancellationOutcome, CancellationRunObservation},
    application_run::{RunReason, RunStatus},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    thread,
};

const KEY: &str = "44444444-4444-4444-8444-444444444444";
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
            client_id: "fixture".into(),
            audience: "fixture-ai".into(),
            scope: "app.fixture.ai".into(),
            client_secret_ref: "secret:fixture.identity".into(),
        },
        profiles: BTreeMap::from([(
            "learning_practice_text".into(),
            ProfileReference {
                id: "practice".into(),
                revision: None,
            },
        )]),
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
fn proof() -> RunCancellationReceipt {
    RunCancellationReceipt {
        schema_version: 1,
        binding: fixture("http://127.0.0.1:1").binding,
        binding_revision: 1,
        dispatch_key: KEY.into(),
        payload_digest: "a".repeat(64),
        outcome: CancellationOutcome::CreationPrevented,
        cancellation_id: Some("55555555-5555-4555-8555-555555555555".into()),
        observed_run: None,
    }
}
fn response(status: u16, body: &str) -> String {
    format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
}
fn server(replies: Vec<Option<String>>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = vec![];
        for reply in replies {
            let end = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < end, "fixture request missing");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = vec![];
            let mut buffer = [0u8; 2048];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(boundary) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..boundary]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|s| s.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= boundary + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            if let Some(reply) = reply {
                stream.write_all(reply.as_bytes()).unwrap();
            }
        }
        requests
    });
    (origin, handle)
}

#[tokio::test]
async fn cancellation_lost_reply_requires_an_explicit_exact_repeat_and_preserves_proof() {
    let expected = proof();
    let body = serde_json::to_string(&expected).unwrap();
    let (origin, handle) = server(vec![
        None,
        Some(response(200, &body)),
        Some(response(200, &body)),
    ]);
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(2)).unwrap();
    let token = credential(&connection);
    assert_eq!(
        client
            .cancel_run_by_key(&connection, &token, KEY, &"a".repeat(64))
            .await
            .unwrap_err(),
        Error::Transport
    );
    let second = client
        .cancel_run_by_key(&connection, &token, KEY, &"a".repeat(64))
        .await
        .unwrap();
    assert_eq!(second, expected);
    assert_eq!(
        client
            .cancel_run_by_key(&connection, &token, KEY, &"a".repeat(64))
            .await
            .unwrap(),
        second
    );
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert!(request.starts_with(&format!(
            "POST /api/v1/application-runs/by-key/{KEY}/cancel HTTP/1.1"
        )));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-service-token"));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            json!({"schema_version":1,"payload_digest":"a".repeat(64)})
        );
    }
}

#[tokio::test]
async fn cancellation_validates_identity_digest_outcome_and_historical_revision() {
    for change in 0..8 {
        let mut receipt = proof();
        match change {
            0 => receipt.binding.stage_id = "99999999-9999-4999-8999-999999999999".into(),
            1 => receipt.dispatch_key = "99999999-9999-4999-8999-999999999999".into(),
            2 => receipt.payload_digest = "b".repeat(64),
            3 => receipt.binding_revision = 3,
            4 => receipt.cancellation_id = None,
            5 => {
                receipt.outcome = CancellationOutcome::NotCancelled;
                receipt.cancellation_id = None;
            }
            6 => {
                receipt.observed_run = Some(CancellationRunObservation {
                    run_id: "66666666-6666-4666-8666-666666666666".into(),
                    attempt_id: "77777777-7777-4777-8777-777777777777".into(),
                    status: RunStatus::Running,
                    reason: None,
                })
            }
            _ => {}
        }
        let (origin, handle) = server(vec![Some(response(
            200,
            &serde_json::to_string(&receipt).unwrap(),
        ))]);
        let mut connection = fixture(&origin);
        connection.profiles.clear(); // Historical metadata needs no current profile pin.
                                     // Keep a valid descriptor with a changed role/profile, not a second identity.
        connection.profiles.insert(
            "other_role".into(),
            ProfileReference {
                id: "other".into(),
                revision: Some(99),
            },
        );
        let result = ApplicationClient::new(&origin, Duration::from_secs(2))
            .unwrap()
            .cancel_run_by_key(&connection, &credential(&connection), KEY, &"a".repeat(64))
            .await;
        if change == 7 {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
        assert_eq!(handle.join().unwrap().len(), 1);
    }
    let mut observed = proof();
    observed.outcome = CancellationOutcome::NotCancelled;
    observed.cancellation_id = None;
    observed.observed_run = Some(CancellationRunObservation {
        run_id: "66666666-6666-4666-8666-666666666666".into(),
        attempt_id: "77777777-7777-4777-8777-777777777777".into(),
        status: RunStatus::Unknown,
        reason: Some(RunReason::DispatchUnknown),
    });
    let (origin, handle) = server(vec![Some(response(
        200,
        &serde_json::to_string(&observed).unwrap(),
    ))]);
    let connection = fixture(&origin);
    assert_eq!(
        ApplicationClient::new(&origin, Duration::from_secs(2))
            .unwrap()
            .cancel_run_by_key(&connection, &credential(&connection), KEY, &"a".repeat(64))
            .await
            .unwrap()
            .outcome,
        CancellationOutcome::NotCancelled
    );
    handle.join().unwrap();
}

#[tokio::test]
async fn cancellation_rejects_foreign_expired_or_invalid_context_before_http() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(1)).unwrap();
    let token = credential(&connection);
    for field in 0..8 {
        let mut wrong = connection.clone();
        match field {
            0 => wrong.binding.binding_id = "99999999-9999-4999-8999-999999999999".into(),
            1 => wrong.binding.application_id = "99999999-9999-4999-8999-999999999999".into(),
            2 => wrong.binding.stage_id = "99999999-9999-4999-8999-999999999999".into(),
            3 => wrong.binding.service_name = "other".into(),
            4 => wrong.auth.client_id = "other".into(),
            5 => wrong.auth.issuer = format!("{origin}/other"),
            6 => wrong.auth.audience = "other".into(),
            _ => wrong.auth.scope = "app.other.ai".into(),
        }
        assert_eq!(
            client
                .cancel_run_by_key(&wrong, &token, KEY, &"a".repeat(64))
                .await
                .unwrap_err(),
            Error::BindingMismatch
        );
    }
    let mut expired = credential(&connection);
    expired.expires = Instant::now() - Duration::from_secs(1);
    assert_eq!(
        client
            .cancel_run_by_key(&connection, &expired, KEY, &"a".repeat(64))
            .await
            .unwrap_err(),
        Error::ExpiredCredential
    );
    for (key, digest) in [
        ("../sibling", "a".repeat(64)),
        (KEY, "private-token".into()),
    ] {
        assert_eq!(
            client
                .cancel_run_by_key(&connection, &token, key, &digest)
                .await
                .unwrap_err(),
            Error::InvalidConfiguration
        );
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn cancellation_bounds_errors_rejects_redirects_and_never_exposes_body_or_secret() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    for reply in [format!("HTTP/1.1 200 fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",MAX_CANCELLATION_RESPONSE_BYTES+1),
        response(200,&"x".repeat(MAX_CANCELLATION_RESPONSE_BYTES+1)),response(500,"private-provider-body synthetic-service-token"),
        format!("HTTP/1.1 302 fixture\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",target.local_addr().unwrap()),
        response(202,&serde_json::to_string(&proof()).unwrap())] {
        let (origin,handle)=server(vec![Some(reply)]);let connection=fixture(&origin);
        let error=ApplicationClient::new(&origin,Duration::from_secs(2)).unwrap().cancel_run_by_key(&connection,&credential(&connection),KEY,&"a".repeat(64)).await.unwrap_err();
        assert!(matches!(error,Error::ResponseTooLarge|Error::Http(302|500)|Error::InvalidResponse));
        assert!(!format!("{error:?} {error}").contains("private-provider-body"));
        assert!(!format!("{error:?} {error}").contains("synthetic-service-token"));
        assert_eq!(handle.join().unwrap().len(),1);
    }
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
