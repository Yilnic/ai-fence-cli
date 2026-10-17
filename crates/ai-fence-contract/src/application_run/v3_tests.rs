use super::*;
use crate::audio_transcription::TranscriptionInput;

fn audio_request() -> SubmitRunRequest {
    let mut request = request();
    request.input = TranscriptionInput {
        audio_sha256: "a".repeat(64),
        source_duration_ms: 2000,
        language: Some("de".into()),
        segment_timestamps: true,
    }
    .into();
    request.input_kind = Some(InputKind::AudioTranscription);
    request.receipt_version = Some(3);
    request.payload_digest = request.canonical_digest().unwrap();
    request
}
fn receipt3() -> RunReceipt {
    let mut receipt = response().receipt.unwrap();
    receipt.schema_version = 3;
    receipt.input_audio_ms = Some(u64::MAX);
    receipt.usage_state = UsageState::Reported;
    receipt.settlement_state = Some(SettlementState::ReconciliationRequired);
    receipt.price_state = PriceState::Estimated;
    receipt.usd_amount = Some("0.125".into());
    receipt.price_snapshot_ref = Some("frozen-price".into());
    receipt
}
#[test]
fn speech_negotiation_and_digest_bind_exact_audio_and_options() {
    let request = audio_request();
    request.validate().unwrap();
    assert_eq!(request.input.max_output_tokens(), 0);
    let raw = serde_json::to_vec(&request).unwrap();
    assert_eq!(parse_request(&raw).unwrap(), request);
    for component in 0..5 {
        let mut other = request.clone();
        let RunInput::Audio(input) = &mut other.input else {
            panic!()
        };
        match component {
            0 => input.audio_sha256 = "b".repeat(64),
            1 => input.source_duration_ms += 1,
            2 => input.language = Some("en".into()),
            3 => input.segment_timestamps = false,
            _ => {
                other.profile_revision += 1;
            }
        }
        assert_ne!(other.canonical_digest().unwrap(), request.payload_digest);
    }
    for (kind, receipt) in [
        (None, None),
        (Some(InputKind::AudioTranscription), None),
        (Some(InputKind::AudioTranscription), Some(2)),
        (Some(InputKind::NativeFunctions), Some(3)),
        (None, Some(3)),
    ] {
        let mut bad = request.clone();
        bad.input_kind = kind;
        bad.receipt_version = receipt;
        assert!(bad.canonical_digest().is_err());
    }
}
#[test]
fn duration_receipts_use_exact_strings_without_token_evidence_or_downgrade() {
    let receipt = receipt3();
    receipt.validate().unwrap();
    let wire = serde_json::to_value(&receipt).unwrap();
    assert_eq!(wire["input_audio_ms"], u64::MAX.to_string());
    assert_eq!(wire["input_tokens"], Value::Null);
    assert!(wire.get("total_tokens").is_none());
    assert_eq!(
        serde_json::from_value::<RunReceipt>(wire.clone()).unwrap(),
        receipt
    );
    for value in [
        json!(u64::MAX),
        json!("01"),
        json!("-1"),
        json!("18446744073709551616"),
    ] {
        let mut bad = wire.clone();
        bad["input_audio_ms"] = value;
        assert!(serde_json::from_value::<RunReceipt>(bad).is_err());
    }
    for field in ["input_audio_ms", "settlement_state"] {
        let mut bad = wire.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<RunReceipt>(bad).is_err());
    }
    for version in [1, 2] {
        let mut bad = wire.clone();
        bad["schema_version"] = json!(version);
        assert!(serde_json::from_value::<RunReceipt>(bad).is_err());
    }
    for cache_write in [false, true] {
        let mut mixed = receipt.clone();
        if cache_write {
            mixed.cache_write_tokens = Some(1);
        } else {
            mixed.total_tokens = Some(1);
        }
        assert!(mixed.validate().is_err());
        assert!(
            serde_json::to_vec(&mixed).is_err(),
            "v3 must not silently drop token evidence"
        );
    }
    let mut mixed = receipt.clone();
    mixed.input_tokens = Some(1);
    assert!(mixed.validate().is_err());
    let mut zero = receipt;
    zero.input_audio_ms = Some(0);
    assert!(zero.validate().is_err());
}
#[test]
fn unknown_duration_remains_null_and_held_without_price_or_fake_tokens() {
    let mut receipt = receipt3();
    receipt.input_audio_ms = None;
    receipt.usage_state = UsageState::Unknown;
    receipt.price_state = PriceState::Unknown;
    receipt.usd_amount = None;
    receipt.price_snapshot_ref = None;
    receipt.settlement_state = Some(SettlementState::HeldUnknown);
    receipt.validate().unwrap();
    let wire = serde_json::to_value(&receipt).unwrap();
    assert_eq!(wire["input_audio_ms"], Value::Null);
    assert_eq!(serde_json::from_value::<RunReceipt>(wire).unwrap(), receipt);
    receipt.settlement_state = Some(SettlementState::Settled);
    assert!(receipt.validate().is_err());
}
