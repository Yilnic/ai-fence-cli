use super::*;
use ai_fence_contract::audio_transcription::{AudioLimits, PreparedAudio, TranscriptionInput};

fn audio() -> PreparedAudio {
    let data = vec![0; 16400];
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    bytes.extend_from_slice(&8200u32.to_le_bytes());
    bytes.extend_from_slice(&16400u32.to_le_bytes());
    bytes.extend_from_slice(b"\x02\0\x10\0data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    PreparedAudio::prepare(
        &bytes,
        &AudioLimits {
            max_input_bytes: 32768,
            max_input_duration_ms: 2000,
        },
    )
    .unwrap()
}
fn pair(connection: &ManagedConnection) -> (SubmitRunRequest, RunResponse) {
    let mut request = request();
    let audio = audio();
    request.input = TranscriptionInput {
        audio_sha256: audio.sha256().into(),
        source_duration_ms: audio.duration_ms(),
        language: Some("de".into()),
        segment_timestamps: true,
    }
    .into();
    request.input_kind = Some(InputKind::AudioTranscription);
    request.receipt_version = Some(3);
    request.payload_digest = request.canonical_digest().unwrap();
    let mut result = completed(connection);
    result.input_kind = request.input_kind;
    result.receipt_version = request.receipt_version;
    result.payload_digest = request.payload_digest.clone();
    let receipt = result.receipt.as_mut().unwrap();
    receipt.schema_version = 3;
    receipt.input_audio_ms = Some(1000);
    receipt.usage_state = UsageState::Reported;
    receipt.settlement_state = Some(SettlementState::Settled);
    receipt.price_state = PriceState::Estimated;
    receipt.usd_amount = Some("0.001".into());
    receipt.price_snapshot_ref = Some("frozen-price".into());
    result.result = Some(
        json!({"text":"PRIVATE_TRANSCRIPT_SENTINEL","language":"de","source_duration_ms":1000,
        "segments":[{"start":0,"end":1,"text":"PRIVATE_TRANSCRIPT_SENTINEL"}]}),
    );
    result.validate(true).unwrap();
    (request, result)
}

#[tokio::test]
async fn transcription_submit_sends_canonical_audio_once_and_recovers_metadata_without_audio_replay(
) {
    let (origin, handle) = server(|origin| {
        let (_, result) = pair(&fixture(origin));
        vec![wire(&result), wire(&result.without_result())]
    });
    let connection = fixture(&origin);
    let credential = credential(&connection);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let (request, expected) = pair(&connection);
    assert_eq!(
        client
            .submit_transcription(&connection, &credential, &request, audio())
            .await
            .unwrap(),
        expected
    );
    let metadata = client
        .run_by_key(
            &connection,
            &credential,
            &request.dispatch_key,
            &request.payload_digest,
        )
        .await
        .unwrap();
    assert_eq!(metadata, expected.without_result());
    let calls = handle.join().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with("POST /api/v1/application-runs/transcriptions "));
    assert!(calls[0].contains("multipart/form-data; boundary="));
    assert!(calls[0].contains(&serde_json::to_string(&request).unwrap()));
    let expected_audio = audio();
    assert!(calls[0]
        .as_bytes()
        .windows(expected_audio.as_bytes().len())
        .any(|window| window == expected_audio.as_bytes()));
    assert!(!calls[1].contains("RIFF") && !calls[1].contains("audio_sha256"));
}

#[tokio::test]
async fn transcription_client_refuses_wrong_audio_or_identity_before_any_http() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let connection = fixture(&origin);
    let credential = credential(&connection);
    let client = ApplicationClient::new(&origin, Duration::from_secs(1)).unwrap();
    let (mut request, _) = pair(&connection);
    assert_eq!(
        client
            .submit_run(&connection, &credential, &request)
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let RunInput::Audio(input) = &mut request.input else {
        panic!()
    };
    input.audio_sha256 = "b".repeat(64);
    request.payload_digest = request.canonical_digest().unwrap();
    assert_eq!(
        client
            .submit_transcription(&connection, &credential, &request, audio())
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let (request, _) = pair(&connection);
    let mut other = connection.clone();
    other.binding_revision += 1;
    assert!(client
        .submit_transcription(&other, &credential, &request, audio())
        .await
        .is_err());
    assert!(matches!(listener.accept(), Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[tokio::test]
async fn transcription_client_rejects_invalid_timestamps_source_and_receipt_downgrades() {
    for case in 0..3 {
        let (origin, handle) = server(|origin| {
            let (_, mut result) = pair(&fixture(origin));
            match case {
                0 => result.result.as_mut().unwrap()["segments"][0]["end"] = json!(2),
                1 => result.result.as_mut().unwrap()["source_duration_ms"] = json!(999),
                _ => result.receipt = completed(&fixture(origin)).receipt,
            }
            vec![wire(&result)]
        });
        let connection = fixture(&origin);
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        let (request, _) = pair(&connection);
        assert_eq!(
            client
                .submit_transcription(&connection, &credential(&connection), &request, audio())
                .await
                .unwrap_err(),
            Error::InvalidResponse
        );
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}
