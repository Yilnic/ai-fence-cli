//! Frozen synthetic wire generated and validated by accepted source2c, not the new DTOs.
use super::*;
fn frozen() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/accepted-2c-v1.json")).unwrap()
}
#[tokio::test]
async fn upgraded_sdk_recovers_accepted_v1_identity_and_numeric_receipts_without_post_or_rewrite() {
    let fixture_data = frozen();
    let saved: SubmitRunRequest = serde_json::from_value(fixture_data["request"].clone()).unwrap();
    saved.validate().unwrap();
    assert_eq!(
        saved.canonical_digest().unwrap(),
        "eec47834d291d9517dc811aaca3b2e0b41e882853a90a144a9b51a470437bec4"
    );
    assert_eq!(saved.input_kind, None);
    assert_eq!(saved.receipt_version, None);
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        fixture_data["request"]
    );
    for name in ["metadata", "unknown_metadata"] {
        let body = fixture_data[name].to_string();
        let (origin, handle) = server(|_| vec![response(200, &body), response(200, &body)]);
        let expected: RunResponse = serde_json::from_value(fixture_data[name].clone()).unwrap();
        let mut connection = fixture(&origin);
        connection.binding = expected.binding.clone();
        connection.binding_revision = expected.binding_revision + 1;
        connection
            .profiles
            .get_mut(&saved.workload_role)
            .unwrap()
            .revision = Some(saved.profile_revision + 1);
        let credential = credential(&connection);
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        let by_key = client
            .run_by_key(
                &connection,
                &credential,
                &saved.dispatch_key,
                &saved.payload_digest,
            )
            .await
            .unwrap();
        let by_id = client
            .run_by_id(&connection, &credential, &expected.run_id)
            .await
            .unwrap();
        assert_eq!(by_key, expected);
        assert_eq!(by_id, expected);
        assert!(by_key.matches_request(&saved));
        assert_eq!(serde_json::to_value(&by_key).unwrap(), fixture_data[name]);
        assert_eq!(by_key.input_kind, None);
        assert_eq!(by_key.receipt_version, None);
        assert!(by_key.result.is_none());
        assert_eq!(by_key.result_delivery, ResultDelivery::NotRetained);
        let receipt = by_key.receipt.unwrap();
        assert_eq!(receipt.schema_version, 1);
        assert_eq!(receipt.total_tokens, None);
        assert_eq!(receipt.cache_write_tokens, None);
        assert_eq!(receipt.input_audio_ms, None);
        assert_eq!(receipt.settlement_state, None);
        if name == "metadata" {
            assert_eq!(receipt.input_tokens, Some(30));
            assert_eq!(receipt.usd_amount.as_deref(), Some("0.0001"));
        } else {
            assert_eq!(receipt.usage_state, UsageState::Unknown);
            assert_eq!(receipt.input_tokens, None);
            assert_eq!(receipt.price_state, PriceState::Unknown);
            assert_eq!(receipt.usd_amount, None);
        }
        let calls = handle.join().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls
            .iter()
            .all(|call| call.starts_with("GET ") && !call.contains("FICTIONAL_SAVED_V1")));
        assert!(calls[0].contains(&saved.dispatch_key));
        assert!(calls[0].contains(&saved.payload_digest));
        assert!(calls[1].contains(&expected.run_id));
        assert_eq!(
            serde_json::to_value(&saved).unwrap(),
            fixture_data["request"]
        );
    }
    // Receipt2 is an explicit choice for a new request. No saved attempt is
    // migrated, renumbered, re-keyed or submitted to repair missing content.
    let mut fresh = saved.clone();
    fresh.operation_id = "99999999-9999-4999-8999-999999999999".into();
    fresh.dispatch_key = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into();
    fresh.binding_revision += 1;
    fresh.profile_revision += 1;
    fresh.receipt_version = Some(2);
    fresh.payload_digest = fresh.canonical_digest().unwrap();
    fresh.validate().unwrap();
    assert_ne!(fresh.payload_digest, saved.payload_digest);
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        fixture_data["request"]
    );
}
