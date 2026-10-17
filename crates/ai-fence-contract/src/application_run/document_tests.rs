use super::*;
use crate::document_input::{DocumentInput, DocumentMediaType, DocumentSource};
fn document_request() -> SubmitRunRequest {
    let mut r = request();
    r.input = DocumentInput {
        source: DocumentSource {
            source_sha256: "a".repeat(64),
            source_bytes: 128,
            media_type: DocumentMediaType::Pdf,
        },
        output: r.input.as_structured().unwrap().clone(),
    }
    .into();
    r.input_kind = Some(InputKind::StructuredDocument);
    r.receipt_version = Some(2);
    r.payload_digest = r.canonical_digest().unwrap();
    r
}
#[test]
fn document_requests_bind_original_source_and_schema_with_explicit_v2_negotiation() {
    let r = document_request();
    r.validate().unwrap();
    assert_eq!(parse_request(&serde_json::to_vec(&r).unwrap()).unwrap(), r);
    for case in 0..6 {
        let mut changed = r.clone();
        let RunInput::Document(i) = &mut changed.input else {
            panic!()
        };
        match case {
            0 => i.source.source_sha256 = "b".repeat(64),
            1 => i.source.source_bytes += 1,
            2 => i.source.media_type = DocumentMediaType::Png,
            3 => i.output.prompt.push('x'),
            4 => i.output.json_schema["description"] = json!("PRIVATE_NEW_SCHEMA"),
            _ => changed.profile_revision += 1,
        }
        assert_ne!(changed.canonical_digest().unwrap(), r.payload_digest);
    }
    for (kind, version) in [
        (None, None),
        (None, Some(2)),
        (Some(InputKind::StructuredDocument), None),
        (Some(InputKind::StructuredDocument), Some(1)),
        (Some(InputKind::StructuredDocument), Some(3)),
        (Some(InputKind::NativeFunctions), Some(2)),
    ] {
        let mut bad = r.clone();
        bad.input_kind = kind;
        bad.receipt_version = version;
        assert!(bad.canonical_digest().is_err());
    }
}
#[test]
fn document_response_v2_preserves_numeric_evidence_and_refuses_receipt_downgrades() {
    let r = document_request();
    let mut v = response();
    v.input_kind = r.input_kind;
    v.receipt_version = r.receipt_version;
    v.payload_digest = r.payload_digest.clone();
    let receipt = v.receipt.as_mut().unwrap();
    receipt.schema_version = 2;
    receipt.usage_state = UsageState::Reported;
    receipt.price_state = PriceState::Estimated;
    receipt.usd_amount = Some("0.001".into());
    receipt.price_snapshot_ref = Some("frozen-price".into());
    receipt.total_tokens = Some(40);
    receipt.input_tokens = Some(30);
    receipt.output_tokens = Some(10);
    receipt.cache_write_tokens = Some(0);
    receipt.settlement_state = Some(SettlementState::Settled);
    v.validate(true).unwrap();
    assert!(v.matches_request(&r));
    let wire = serde_json::to_value(&v).unwrap();
    assert_eq!(wire["receipt"]["input_tokens"], json!("30"));
    assert_eq!(serde_json::from_value::<RunResponse>(wire).unwrap(), v);
    for version in [None, Some(1), Some(3)] {
        let mut bad = v.clone();
        bad.receipt_version = version;
        assert!(bad.validate(true).is_err());
    }
    let mut old = r.clone();
    old.input_kind = None;
    old.receipt_version = None;
    assert!(old.canonical_digest().is_err());
}
