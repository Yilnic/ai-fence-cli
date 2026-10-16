use super::*;

fn proof() -> RunCancellationReceipt {
    RunCancellationReceipt {
        schema_version: 1,
        binding: BindingIdentity {
            binding_id: "11111111-1111-4111-8111-111111111111".into(),
            application_id: "22222222-2222-4222-8222-222222222222".into(),
            stage_id: "33333333-3333-4333-8333-333333333333".into(),
            service_name: "ai".into(),
        },
        binding_revision: 1,
        dispatch_key: "44444444-4444-4444-8444-444444444444".into(),
        payload_digest: "a".repeat(64),
        outcome: CancellationOutcome::CreationPrevented,
        cancellation_id: Some("55555555-5555-4555-8555-555555555555".into()),
        observed_run: None,
    }
}
fn observation(status: RunStatus, reason: Option<RunReason>) -> CancellationRunObservation {
    CancellationRunObservation {
        run_id: "66666666-6666-4666-8666-666666666666".into(),
        attempt_id: "77777777-7777-4777-8777-777777777777".into(),
        status,
        reason,
    }
}

#[test]
fn strict_cancellation_request_rejects_duplicates_unknowns_versions_and_oversize() {
    let bytes = serde_json::to_vec(&CancelRunRequest {
        schema_version: 1,
        payload_digest: "a".repeat(64),
    })
    .unwrap();
    assert!(parse_cancellation_request(&bytes).is_ok());
    let wire = String::from_utf8(bytes).unwrap();
    for invalid in [
        wire.replace(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
        ),
        wire.replace(
            "\"payload_digest\":",
            "\"provider_key\":\"private-key\",\"payload_digest\":",
        ),
        wire.replace("\"schema_version\":1", "\"schema_version\":2"),
        wire.replace(&"a".repeat(64), &"A".repeat(64)),
        "null".into(),
        format!("{wire} {{}}"),
        "x".repeat(MAX_CANCELLATION_REQUEST_BYTES + 1),
    ] {
        let error = parse_cancellation_request(invalid.as_bytes()).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("private-key"));
    }
}

#[test]
fn creation_proof_and_non_cancellation_observation_have_distinct_consistency_rules() {
    let mut value = proof();
    value.validate().unwrap();
    value.observed_run = Some(observation(
        RunStatus::Rejected,
        Some(RunReason::AdmissionDenied),
    ));
    value.validate().unwrap();
    for status in [
        RunStatus::Registered,
        RunStatus::Running,
        RunStatus::Completed,
        RunStatus::Failed,
        RunStatus::Unknown,
    ] {
        let reason = match status {
            RunStatus::Failed => Some(RunReason::ProviderFailed),
            RunStatus::Unknown => Some(RunReason::DispatchUnknown),
            _ => None,
        };
        value.observed_run = Some(observation(status, reason));
        assert!(value.validate().is_err());
    }
    value = proof();
    value.outcome = CancellationOutcome::NotCancelled;
    assert!(value.validate().is_err());
    value.cancellation_id = None;
    assert!(value.validate().is_err());
    value.observed_run = Some(observation(RunStatus::Running, None));
    value.validate().unwrap();
    value.observed_run = Some(observation(
        RunStatus::Unknown,
        Some(RunReason::DispatchUnknown),
    ));
    value.validate().unwrap();
    value.observed_run.as_mut().unwrap().reason = None;
    assert!(value.validate().is_err());
}

#[test]
fn cancellation_receipt_is_bound_to_one_canonical_identity_key_and_digest() {
    let original = proof();
    assert!(original.matches_attempt(
        &original.binding,
        &original.dispatch_key,
        &original.payload_digest
    ));
    assert!(!original.matches_attempt(&original.binding, "other", &original.payload_digest));
    assert!(!original.matches_attempt(&original.binding, &original.dispatch_key, &"b".repeat(64)));
    for field in 0..7 {
        let mut changed = original.clone();
        match field {
            0 => changed.binding.binding_id = "00000000-0000-0000-0000-000000000000".into(),
            1 => changed.binding.stage_id = "not-uuid".into(),
            2 => changed.binding.service_name = "ai/sibling".into(),
            3 => changed.dispatch_key = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into(),
            4 => changed.payload_digest = "private-key".into(),
            5 => changed.binding_revision = 0,
            _ => changed.cancellation_id = Some("private-key".into()),
        }
        assert!(changed.validate().is_err());
    }
    let mut foreign = original.binding.clone();
    foreign.stage_id = "88888888-8888-4888-8888-888888888888".into();
    assert!(!original.matches_attempt(&foreign, &original.dispatch_key, &original.payload_digest));
    let mut wire = serde_json::to_value(original).unwrap();
    wire["binding"]["provider_key"] = "private-key".into();
    assert!(serde_json::from_value::<RunCancellationReceipt>(wire).is_err());
}

#[test]
fn cancellation_debug_never_prints_unvalidated_caller_or_producer_strings() {
    let request = CancelRunRequest {
        schema_version: 1,
        payload_digest: "private-key".into(),
    };
    let mut value = proof();
    value.binding.service_name = "private-key".into();
    value.cancellation_id = Some("private-key".into());
    let mut observed = observation(RunStatus::Running, None);
    observed.run_id = "private-key".into();
    assert!(!format!("{request:?} {value:?} {observed:?}").contains("private-key"));
}
