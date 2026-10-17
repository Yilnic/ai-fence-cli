use super::*;
use crate::application_native::{NativeTurnInput, NativeTurnOutput};

fn native_request() -> SubmitRunRequest {
    let mut request = request();
    let input: NativeTurnInput = serde_json::from_value(json!({
        "instructions":"PRIVATE_NATIVE_SENTINEL", "history":[{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"Review fictional evidence"}]}],
        "tools":[{"type":"function","name":"mc_tool_0","strict":false,"parameters":{"type":"object"}}],
        "max_output_tokens":512
    })).unwrap();
    request.input = input.into();
    request.input_kind = Some(InputKind::NativeFunctions);
    request.receipt_version = Some(2);
    request.payload_digest = request.canonical_digest().unwrap();
    request
}
fn receipt2() -> RunReceipt {
    let mut receipt = response().receipt.unwrap();
    receipt.schema_version = 2;
    receipt.input_tokens = Some(u64::MAX - 10);
    receipt.output_tokens = Some(10);
    receipt.total_tokens = Some(u64::MAX);
    receipt.cached_input_tokens = Some(2);
    receipt.cache_write_tokens = Some(3);
    receipt.reasoning_tokens = Some(4);
    receipt.usage_state = UsageState::Reported;
    receipt.settlement_state = Some(SettlementState::ReconciliationRequired);
    receipt.price_state = PriceState::Estimated;
    receipt.usd_amount = Some("0.0000000000005".into());
    receipt.price_snapshot_ref = Some("frozen-price".into());
    receipt
}

#[test]
fn legacy_wire_has_no_mode_or_v2_receipt_fields_and_keeps_numeric_counts() {
    let request = request();
    assert_eq!(
        request.payload_digest,
        "656716560df1410118e242bd007aa5e26dfd7ab406f096bd070eee6918f724c3"
    );
    let wire = serde_json::to_value(&request).unwrap();
    assert!(wire.get("input_kind").is_none());
    assert!(wire.get("receipt_version").is_none());
    let mut receipt = response().receipt.unwrap();
    receipt.usage_state = UsageState::Reported;
    receipt.input_tokens = Some(123);
    let wire = serde_json::to_value(&receipt).unwrap();
    assert_eq!(wire["input_tokens"], 123);
    for key in ["total_tokens", "cache_write_tokens", "settlement_state"] {
        assert!(wire.get(key).is_none());
    }
    assert_eq!(serde_json::from_value::<RunReceipt>(wire).unwrap(), receipt);
}

#[test]
fn native_run_requires_explicit_mode_and_receipt_negotiation_bound_into_digest() {
    let request = native_request();
    let bytes = serde_json::to_vec(&request).unwrap();
    assert_eq!(parse_request(&bytes).unwrap(), request);
    assert!(!format!("{request:?}").contains("PRIVATE_NATIVE_SENTINEL"));
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["input_kind"], "native_functions");
    assert_eq!(wire["receipt_version"], 2);
    for (kind, version) in [
        (None, None),
        (Some(InputKind::NativeFunctions), None),
        (None, Some(2)),
        (Some(InputKind::StructuredText), Some(2)),
        (Some(InputKind::NativeFunctions), Some(1)),
        (Some(InputKind::NativeFunctions), Some(3)),
    ] {
        let mut changed = request.clone();
        changed.input_kind = kind;
        changed.receipt_version = version;
        assert!(changed.canonical_digest().is_err());
    }
    let mut changed = request.clone();
    if let RunInput::Native(input) = &mut changed.input {
        input.instructions.push('!');
    }
    assert_ne!(changed.canonical_digest().unwrap(), request.payload_digest);
    let mut text = super::request();
    text.receipt_version = Some(2);
    assert_ne!(
        text.canonical_digest().unwrap(),
        super::request().payload_digest
    );
    text.payload_digest = text.canonical_digest().unwrap();
    text.validate().unwrap();
}

#[test]
fn native_wire_rejects_null_selectors_hybrid_input_duplicate_and_unknown_fields() {
    let request = native_request();
    let value = serde_json::to_value(request).unwrap();
    for key in ["input_kind", "receipt_version"] {
        let mut bad = value.clone();
        bad[key] = Value::Null;
        assert!(parse_request(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    for key in [
        "prompt",
        "json_schema",
        "schema_name",
        "previous_response_id",
    ] {
        let mut bad = value.clone();
        bad["input"][key] = json!("PRIVATE_HYBRID_SENTINEL");
        assert!(parse_request(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    let raw = serde_json::to_string(&value).unwrap();
    let bad = raw.replacen(
        "\"receipt_version\":2",
        "\"receipt_version\":2,\"receipt_version\":2",
        1,
    );
    assert!(parse_request(bad.as_bytes()).is_err());
}

#[test]
fn receipt_v2_preserves_full_counters_as_strings_and_tiny_known_cost() {
    let receipt = receipt2();
    receipt.validate().unwrap();
    let wire = serde_json::to_value(&receipt).unwrap();
    assert_eq!(wire["total_tokens"], u64::MAX.to_string());
    assert_eq!(wire["input_tokens"], (u64::MAX - 10).to_string());
    assert_eq!(wire["cache_write_tokens"], "3");
    assert_eq!(wire["settlement_state"], "reconciliation_required");
    assert_eq!(wire["usd_amount"], "0.0000000000005");
    for key in [
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "total_tokens",
    ] {
        assert!(wire[key].is_string());
    }
    assert_eq!(serde_json::from_value::<RunReceipt>(wire).unwrap(), receipt);
}

#[test]
fn receipt_counter_encoding_cannot_mix_versions_round_or_accept_noncanonical_strings() {
    let original = serde_json::to_value(receipt2()).unwrap();
    for bad in [
        json!(1),
        json!(1.0),
        json!("00"),
        json!("+1"),
        json!("-1"),
        json!("1e3"),
        json!("18446744073709551616"),
        json!("1.0"),
    ] {
        let mut wire = original.clone();
        wire["input_tokens"] = bad;
        assert!(serde_json::from_value::<RunReceipt>(wire).is_err());
    }
    for key in ["total_tokens", "cache_write_tokens", "settlement_state"] {
        let mut wire = original.clone();
        wire.as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<RunReceipt>(wire).is_err());
        let mut legacy = serde_json::to_value(response().receipt.unwrap()).unwrap();
        legacy[key] = Value::Null;
        assert!(serde_json::from_value::<RunReceipt>(legacy).is_err());
    }
    let mut legacy = serde_json::to_value(response().receipt.unwrap()).unwrap();
    legacy["input_tokens"] = json!("1");
    assert!(serde_json::from_value::<RunReceipt>(legacy).is_err());
    let raw = serde_json::to_string(&original).unwrap();
    let duplicate = raw.replacen(
        "\"cache_write_tokens\":\"3\"",
        "\"cache_write_tokens\":\"3\",\"cache_write_tokens\":\"3\"",
        1,
    );
    assert!(serde_json::from_str::<RunReceipt>(&duplicate).is_err());
    let mut downgrade = receipt2();
    downgrade.schema_version = 1;
    assert!(downgrade.validate().is_err());
    assert!(serde_json::to_value(downgrade).is_err());
}

#[test]
fn receipt_v2_enforces_complete_totals_subsets_and_unknown_accounting_state() {
    for case in [
        "missing_total",
        "wrong_total",
        "cached",
        "write",
        "subsets",
        "reasoning",
        "overflow",
        "unknown_with_counts",
        "missing_state",
        "settled_without_price",
    ] {
        let mut receipt = receipt2();
        match case {
            "missing_total" => receipt.total_tokens = None,
            "wrong_total" => receipt.total_tokens = Some(0),
            "cached" => receipt.cached_input_tokens = Some(u64::MAX),
            "write" => receipt.cache_write_tokens = Some(u64::MAX),
            "subsets" => {
                receipt.cached_input_tokens = receipt.input_tokens;
                receipt.cache_write_tokens = Some(1);
            }
            "reasoning" => receipt.reasoning_tokens = Some(11),
            "overflow" => {
                receipt.input_tokens = Some(u64::MAX);
                receipt.output_tokens = Some(1);
                receipt.total_tokens = Some(0);
            }
            "unknown_with_counts" => receipt.usage_state = UsageState::Unknown,
            "missing_state" => receipt.settlement_state = None,
            "settled_without_price" => {
                receipt.settlement_state = Some(SettlementState::Settled);
                receipt.price_state = PriceState::Unknown;
                receipt.usd_amount = None;
                receipt.price_snapshot_ref = None;
            }
            _ => unreachable!(),
        }
        assert!(receipt.validate().is_err(), "{case}");
    }
    let mut unknown = response().receipt.unwrap();
    unknown.schema_version = 2;
    unknown.settlement_state = Some(SettlementState::HeldUnknown);
    unknown.validate().unwrap();
    let wire = serde_json::to_value(&unknown).unwrap();
    assert!(wire["total_tokens"].is_null());
    assert!(wire["usd_amount"].is_null());
    assert_eq!(serde_json::from_value::<RunReceipt>(wire).unwrap(), unknown);
}

#[test]
fn receipt_v2_known_usd_covers_finite_extremes_without_inventing_underflow_zero() {
    for amount in [0.0, f64::from_bits(1), 1e-30, 1e100, f64::MAX] {
        let mut receipt = receipt2();
        receipt.usd_amount = Some(amount.to_string());
        receipt.validate().unwrap();
        assert_eq!(
            receipt.usd_amount.as_ref().unwrap().parse::<f64>().unwrap(),
            amount
        );
    }
    for amount in ["NaN", "inf", "1e3", "-0", "+1", "01", ".1", "0.", "1.0.0"] {
        let mut receipt = receipt2();
        receipt.usd_amount = Some(amount.into());
        assert!(receipt.validate().is_err(), "{amount}");
    }
    let mut receipt = receipt2();
    receipt.usd_amount = Some(format!("0.{}1", "0".repeat(324)));
    assert!(receipt.validate().is_err());
}

#[test]
fn native_response_binds_negotiation_and_metadata_keeps_no_result() {
    let request = native_request();
    let mut result = response();
    result.payload_digest = request.payload_digest.clone();
    result.input_kind = request.input_kind;
    result.receipt_version = request.receipt_version;
    result.receipt = Some(receipt2());
    let output:NativeTurnOutput=serde_json::from_value(json!({"output":[{"type":"function_call","call_id":"call_new","name":"mc_tool_0","arguments":"{}"}]})).unwrap();
    output
        .validate_for(request.input.as_native().unwrap())
        .unwrap();
    result.result = Some(serde_json::to_value(output).unwrap());
    result.validate(true).unwrap();
    assert!(result.matches_request(&request));
    let metadata = result.clone().without_result();
    metadata.validate(false).unwrap();
    assert!(metadata.result.is_none());
    assert!(metadata.matches_request(&request));
    let mut wrong = result.clone();
    wrong.receipt = Some(response().receipt.unwrap());
    assert!(wrong.validate(true).is_err());
    let mut wrong = result;
    wrong.input_kind = None;
    assert!(!wrong.matches_request(&request));
}

#[test]
fn inline_results_reject_duplicate_json_members_before_typed_validation() {
    let raw = serde_json::to_string(&response()).unwrap();
    let duplicate = raw.replacen(
        "\"answer\":\"private generated answer\"",
        "\"answer\":\"first\",\"answer\":\"second\"",
        1,
    );
    assert_ne!(raw, duplicate);
    assert!(serde_json::from_str::<RunResponse>(&duplicate).is_err());
}
