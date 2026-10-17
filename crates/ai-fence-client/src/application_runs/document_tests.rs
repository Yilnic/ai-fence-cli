use super::*;
use ai_fence_contract::document_input::{
    DocumentInput, DocumentLimits, DocumentMediaType, PreparedDocument,
};
fn document() -> PreparedDocument {
    PreparedDocument::prepare(
        b"%PDF-1.7\nPRIVATE_FICTIONAL_DOCUMENT\n%%EOF",
        DocumentMediaType::Pdf,
        &DocumentLimits {
            max_input_bytes: 1024,
            allowed_media_types: vec![DocumentMediaType::Pdf],
        },
    )
    .unwrap()
}
fn pair(c: &ManagedConnection) -> (SubmitRunRequest, RunResponse) {
    let mut r = request();
    r.input = DocumentInput {
        source: document().source().clone(),
        output: r.input.as_structured().unwrap().clone(),
    }
    .into();
    r.input_kind = Some(InputKind::StructuredDocument);
    r.receipt_version = Some(2);
    r.payload_digest = r.canonical_digest().unwrap();
    let mut v = completed(c);
    v.input_kind = r.input_kind;
    v.receipt_version = r.receipt_version;
    v.payload_digest = r.payload_digest.clone();
    let receipt = v.receipt.as_mut().unwrap();
    receipt.schema_version = 2;
    receipt.settlement_state = Some(SettlementState::HeldUnknown);
    v.validate(true).unwrap();
    (r, v)
}
#[tokio::test]
async fn document_sdk_sends_original_file_once_and_recovers_only_metadata_by_original_key() {
    let (origin, handle) = server(|origin| {
        let (_, v) = pair(&fixture(origin));
        vec![wire(&v), wire(&v.without_result())]
    });
    let c = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let (r, v) = pair(&c);
    let original = serde_json::to_vec(&r).unwrap();
    assert_eq!(
        client
            .submit_document(&c, &credential(&c), &r, document())
            .await
            .unwrap(),
        v
    );
    assert_eq!(
        client
            .run_by_key(&c, &credential(&c), &r.dispatch_key, &r.payload_digest)
            .await
            .unwrap(),
        v.without_result()
    );
    assert_eq!(serde_json::to_vec(&r).unwrap(), original);
    let calls = handle.join().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with("POST /api/v1/application-runs/documents "));
    assert!(calls[0].contains("filename=\"document.pdf\""));
    assert!(calls[0].contains("PRIVATE_FICTIONAL_DOCUMENT"));
    assert!(calls[1].starts_with("GET "));
    assert!(!calls[1].contains("PRIVATE_FICTIONAL_DOCUMENT"));
}
#[tokio::test]
async fn document_sdk_refuses_wrong_source_mode_and_binding_before_network() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let c = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(1)).unwrap();
    let (mut r, _) = pair(&c);
    assert_eq!(
        client
            .submit_run(&c, &credential(&c), &r)
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let RunInput::Document(input) = &mut r.input else {
        panic!()
    };
    input.source.source_sha256 = "b".repeat(64);
    r.payload_digest = r.canonical_digest().unwrap();
    assert_eq!(
        client
            .submit_document(&c, &credential(&c), &r, document())
            .await
            .unwrap_err(),
        Error::InvalidConfiguration
    );
    let (r, _) = pair(&c);
    let mut other = c.clone();
    other.binding_revision += 1;
    assert!(client
        .submit_document(&other, &credential(&c), &r, document())
        .await
        .is_err());
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}
#[tokio::test]
async fn document_sdk_never_retries_http_failures_and_refuses_receipt_or_source_identity_changes() {
    for case in 0..3 {
        let (origin, handle) = server(|origin| {
            let (_, mut v) = pair(&fixture(origin));
            match case {
                0 => v.payload_digest = "b".repeat(64),
                1 => v.receipt_version = Some(1),
                _ => return vec![response(503, "PRIVATE_PROVIDER_FAILURE")],
            };
            vec![wire(&v)]
        });
        let c = fixture(&origin);
        let (r, _) = pair(&c);
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        assert!(client
            .submit_document(&c, &credential(&c), &r, document())
            .await
            .is_err());
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}
