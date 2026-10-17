use super::*;
use serde_json::json;

#[path = "v2_tests.rs"]
mod v2;
#[path = "v3_tests.rs"]
mod v3;

fn request() -> SubmitRunRequest {
    let mut request = SubmitRunRequest {
        schema_version: 1,
        binding_revision: 2,
        workload_role: "learning_practice_text".into(),
        profile_id: "practice-profile".into(),
        profile_revision: 3,
        operation_id: "11111111-1111-4111-8111-111111111111".into(),
        attempt_number: 1,
        dispatch_key: "22222222-2222-4222-8222-222222222222".into(),
        payload_digest: String::new(),
        funding: Funding::ApplicationApi,
        retention: Retention::MetadataOnly,
        input_kind: None,
        receipt_version: None,
        input: StructuredTextInput {
            instructions: "Teach with evidence".into(),
            prompt: "Private question: 学习?".into(),
            schema_name: "practice".into(),
            json_schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
            max_output_tokens: 512,
        }.into(),
    };
    request.payload_digest = request.canonical_digest().unwrap();
    request
}
fn response() -> RunResponse {
    let request = request();
    let binding = BindingIdentity {
        binding_id: "33333333-3333-4333-8333-333333333333".into(),
        application_id: "44444444-4444-4444-8444-444444444444".into(),
        stage_id: "55555555-5555-4555-8555-555555555555".into(),
        service_name: "ai".into(),
    };
    let receipt = RunReceipt {
        input_audio_ms: None,
        schema_version: 1,
        run_id: "66666666-6666-4666-8666-666666666666".into(),
        attempt_id: "77777777-7777-4777-8777-777777777777".into(),
        provider_call_id: "88888888-8888-4888-8888-888888888888".into(),
        binding: binding.clone(),
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
        binding,
        binding_revision: 2,
        operation_id: request.operation_id.clone(),
        attempt_number: 1,
        dispatch_key: request.dispatch_key.clone(),
        payload_digest: request.payload_digest.clone(),
        workload_role: request.workload_role.clone(),
        profile_id: request.profile_id.clone(),
        profile_revision: 3,
        policy_revision: 4,
        input_kind: None,
        receipt_version: None,
        status: RunStatus::Completed,
        reason: None,
        receipt: Some(receipt),
        result_delivery: ResultDelivery::Inline,
        result: Some(json!({"answer":"private generated answer"})),
    }
}

#[test]
fn canonical_digest_has_a_stable_cross_language_vector_and_excludes_only_key_and_digest() {
    let original = request();
    // Independently calculated from UTF-8, recursively sorted, compact JSON.
    assert_eq!(
        original.payload_digest,
        "656716560df1410118e242bd007aa5e26dfd7ab406f096bd070eee6918f724c3"
    );
    let mut changed = original.clone();
    changed.dispatch_key = "99999999-9999-4999-8999-999999999999".into();
    changed.payload_digest = "0".repeat(64);
    assert_eq!(changed.canonical_digest().unwrap(), original.payload_digest);
    let value = serde_json::to_value(&original).unwrap();
    for (key, replacement) in [
        ("schema_version", json!(2)),
        ("binding_revision", json!(3)),
        ("workload_role", json!("property_document_text")),
        ("profile_id", json!("other-profile")),
        ("profile_revision", json!(4)),
        (
            "operation_id",
            json!("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        ),
        ("attempt_number", json!(2)),
    ] {
        let mut changed = value.clone();
        changed[key] = replacement;
        let changed: SubmitRunRequest = serde_json::from_value(changed).unwrap();
        // Invalid versions are rejected before hashing; all valid fields bind.
        assert_ne!(
            changed.canonical_digest().ok().as_deref(),
            Some(original.payload_digest.as_str())
        );
    }
    let mut changed = original.clone();
    changed.input.as_structured_mut().unwrap().prompt.push('!');
    assert_ne!(changed.canonical_digest().unwrap(), original.payload_digest);
    assert_eq!(changed.validate(), Err(ValidationError::DigestMismatch));
}

#[test]
fn untrusted_json_rejects_duplicate_unknown_trailing_and_oversized_private_fields() {
    let request = request();
    let raw = serde_json::to_string(&request).unwrap();
    assert_eq!(parse_request(raw.as_bytes()).unwrap(), request);
    for invalid in [
        raw.replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
            1,
        ),
        raw.replacen(
            "\"type\":\"string\"",
            "\"type\":\"string\",\"type\":\"object\"",
            1,
        ),
        raw.replacen(
            "\"max_output_tokens\":512",
            "\"max_output_tokens\":512,\"provider_key\":\"private-secret\"",
            1,
        ),
        format!("{raw} null"),
    ] {
        assert_eq!(
            parse_request(invalid.as_bytes()),
            Err(ValidationError::InvalidRequest)
        );
    }
    assert_eq!(
        parse_request(&vec![b' '; MAX_REQUEST_BYTES + 1]),
        Err(ValidationError::TooLarge)
    );
    let mut oversized = request.clone();
    oversized.input.as_structured_mut().unwrap().prompt = "密".repeat(MAX_PROMPT_BYTES / 3 + 1);
    assert_eq!(oversized.canonical_digest(), Err(ValidationError::TooLarge));
    oversized.input.as_structured_mut().unwrap().prompt = "ok".into();
    oversized.input.as_structured_mut().unwrap().instructions =
        "a".repeat(MAX_INSTRUCTIONS_BYTES + 1);
    assert_eq!(oversized.canonical_digest(), Err(ValidationError::TooLarge));
    oversized
        .input
        .as_structured_mut()
        .unwrap()
        .instructions
        .clear();
    oversized.input.as_structured_mut().unwrap().json_schema =
        json!({"description":"x".repeat(MAX_SCHEMA_BYTES)});
    assert_eq!(oversized.canonical_digest(), Err(ValidationError::TooLarge));
    assert!(!format!("{request:?}").contains("Private question"));
    assert!(!ValidationError::InvalidRequest
        .to_string()
        .contains("private-secret"));
}

#[test]
fn text_request_cannot_select_provider_tools_funding_or_noncanonical_identity() {
    let mut value = serde_json::to_value(request()).unwrap();
    for key in [
        "model",
        "provider_url",
        "provider_key",
        "headers",
        "tools",
        "funding_identity",
    ] {
        value[key] = json!("forbidden");
        assert!(serde_json::from_value::<SubmitRunRequest>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove(key);
    }
    for invalid in ["chatgpt_subscription", "owner_grant", "direct"] {
        value["funding"] = json!(invalid);
        assert!(serde_json::from_value::<SubmitRunRequest>(value.clone()).is_err());
    }
    for id in [
        "00000000-0000-0000-0000-000000000000",
        "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
        "../one",
    ] {
        let mut changed = request();
        changed.dispatch_key = id.into();
        assert_eq!(
            changed.canonical_digest(),
            Err(ValidationError::InvalidRequest)
        );
    }
    for attempt in [0, 4, 255] {
        let mut changed = request();
        changed.attempt_number = attempt;
        assert_eq!(
            changed.canonical_digest(),
            Err(ValidationError::InvalidRequest)
        );
    }
}

#[test]
fn receipt_unknowns_are_null_and_cannot_masquerade_as_zero_or_foreign_cost() {
    let mut response = response();
    response.validate(true).unwrap();
    let receipt = response.receipt.as_mut().unwrap();
    let wire = serde_json::to_value(&*receipt).unwrap();
    assert!(wire["usd_amount"].is_null() && wire["input_tokens"].is_null());
    receipt.input_tokens = Some(0);
    assert_eq!(receipt.validate(), Err(ValidationError::InvalidResponse));
    receipt.usage_state = UsageState::Reported;
    receipt.validate().unwrap();
    receipt.usd_amount = Some("0".into());
    assert!(receipt.validate().is_err());
    receipt.price_state = PriceState::Settled;
    receipt.price_snapshot_ref = Some("fixture-price-v1".into());
    receipt.validate().unwrap();
    for invalid in ["-1", "NaN", "0.1e3", "00.01", "1.", "1.0000000000001"] {
        receipt.usd_amount = Some(invalid.into());
        assert!(receipt.validate().is_err());
    }
    receipt.usd_amount = Some("0.000001".into());
    receipt.policy_revision += 1;
    assert_eq!(
        response.validate(true),
        Err(ValidationError::IdentityMismatch)
    );
}

#[test]
fn result_delivery_distinguishes_authoritative_inline_from_lost_content_without_retry() {
    let response = response();
    assert!(response.matches_request(&request()));
    response.validate(true).unwrap();
    assert!(response.validate(false).is_err());
    assert!(!format!("{response:?}").contains("private generated answer"));
    let metadata = response.clone().without_result();
    metadata.validate(false).unwrap();
    assert_eq!(metadata.result_delivery, ResultDelivery::NotRetained);
    assert!(!serde_json::to_string(&metadata)
        .unwrap()
        .contains("private generated answer"));
    for status in [
        RunStatus::Registered,
        RunStatus::Running,
        RunStatus::Failed,
        RunStatus::Rejected,
        RunStatus::Unknown,
    ] {
        let mut invalid = response.clone();
        invalid.status = status;
        assert!(invalid.validate(true).is_err());
    }
    let mut invalid = metadata;
    invalid.result = Some(json!("private replay"));
    assert!(invalid.validate(false).is_err());
    let mut nullable = response;
    nullable.result = Some(Value::Null);
    let decoded: RunResponse =
        serde_json::from_slice(&serde_json::to_vec(&nullable).unwrap()).unwrap();
    assert_eq!(decoded.result, Some(Value::Null));
    decoded.validate(true).unwrap();
}

#[path = "document_tests.rs"]
mod documents;
