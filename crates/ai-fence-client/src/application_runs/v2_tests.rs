use super::*;
use ai_fence_contract::application_native::{NativeTurnInput, NativeTurnOutput};

fn pair(connection: &ManagedConnection) -> (SubmitRunRequest, RunResponse) {
    let mut request = request();
    let input:NativeTurnInput=serde_json::from_value(json!({"instructions":"PRIVATE_NATIVE_INSTRUCTION_SENTINEL",
        "history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"PRIVATE_NATIVE_VISIT_SENTINEL"}]}],
        "tools":[{"type":"function","name":"mc_tool_0","strict":false,"parameters":{"type":"object"}}],"max_output_tokens":512})).unwrap();
    request.input = input.into();
    request.input_kind = Some(InputKind::NativeFunctions);
    request.receipt_version = Some(2);
    request.payload_digest = request.canonical_digest().unwrap();
    let mut result = completed(connection);
    result.input_kind = request.input_kind;
    result.receipt_version = request.receipt_version;
    result.payload_digest = request.payload_digest.clone();
    let receipt = result.receipt.as_mut().unwrap();
    receipt.schema_version = 2;
    receipt.usage_state = UsageState::Reported;
    receipt.input_tokens = Some(30);
    receipt.output_tokens = Some(10);
    receipt.total_tokens = Some(40);
    receipt.cached_input_tokens = Some(2);
    receipt.cache_write_tokens = Some(3);
    receipt.reasoning_tokens = Some(4);
    receipt.settlement_state = Some(SettlementState::ReconciliationRequired);
    receipt.price_state = PriceState::Estimated;
    receipt.usd_amount = Some("0.0000000000005".into());
    receipt.price_snapshot_ref = Some("frozen-price".into());
    result.result = Some(json!({"output":[
        {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"PRIVATE_ENCRYPTED_SENTINEL"},
        {"type":"function_call","call_id":"call_1","name":"mc_tool_0","arguments":"{ \"source\":\"PRIVATE_ARGUMENT_SENTINEL\" }"}
    ]}));
    result.validate(true).unwrap();
    (request, result)
}

#[tokio::test]
async fn native_submit_and_metadata_preserve_protocol_and_full_receipt_without_replay() {
    let (origin, handle) = server(|origin| {
        let (_, result) = pair(&fixture(origin));
        vec![wire(&result), wire(&result.without_result())]
    });
    let connection = fixture(&origin);
    let credential = credential(&connection);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let (request, expected) = pair(&connection);
    let result = client
        .submit_run(&connection, &credential, &request)
        .await
        .unwrap();
    assert_eq!(result, expected);
    let native: NativeTurnOutput = serde_json::from_value(result.result.unwrap()).unwrap();
    native
        .validate_for(request.input.as_native().unwrap())
        .unwrap();
    assert!(!format!("{native:?}").contains("SENTINEL"));
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
    assert!(metadata.matches_request(&request));
    assert_eq!(metadata.receipt.unwrap().total_tokens, Some(40));
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    let sent: SubmitRunRequest =
        serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(sent, request);
    assert!(!requests[1].contains("PRIVATE_"));
}

#[tokio::test]
async fn native_client_rejects_downgraded_receipt_wrong_mode_and_invalid_native_output() {
    for case in [
        "receipt_downgrade",
        "mode",
        "foreign_call",
        "invalid_arguments",
        "duplicate_item_key",
        "user_output",
    ] {
        let (origin, handle) = server(|origin| {
            let (_, mut result) = pair(&fixture(origin));
            match case {
                "receipt_downgrade" => result.receipt = completed(&fixture(origin)).receipt,
                "mode" => result.input_kind = None,
                "foreign_call" => {
                    result.result.as_mut().unwrap()["output"][1]["name"] = json!("unlisted_tool")
                }
                "invalid_arguments" => {
                    result.result.as_mut().unwrap()["output"][1]["arguments"] =
                        json!("{\"n\":1,\"n\":2}")
                }
                "user_output" => {
                    result.result = Some(
                        json!({"output":[{"type":"message","role":"user","content":[{"type":"input_text","text":"PRIVATE_SENTINEL"}]}]}),
                    )
                }
                "duplicate_item_key" => {}
                _ => unreachable!(),
            }
            let body = wire(&result);
            vec![if case == "duplicate_item_key" {
                body.replacen(
                    "\"name\":\"mc_tool_0\"",
                    "\"name\":\"mc_tool_0\",\"name\":\"mc_tool_0\"",
                    1,
                )
            } else {
                body
            }]
        });
        let connection = fixture(&origin);
        let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
        let (request, _) = pair(&connection);
        let error = client
            .submit_run(&connection, &credential(&connection), &request)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            if case == "mode" {
                Error::RunMismatch
            } else {
                Error::InvalidResponse
            },
            "{case}"
        );
        assert!(!format!("{error:?} {error}").contains("PRIVATE_"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn native_lost_submit_reply_recovers_only_matching_v2_metadata_with_one_post() {
    let (origin, handle) = server(|origin| {
        let (_, result) = pair(&fixture(origin));
        vec![String::new(), wire(&result.without_result())]
    });
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let (request, _) = pair(&connection);
    let credential = credential(&connection);
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
    assert!(metadata.matches_request(&request));
    assert!(metadata.result.is_none());
    assert_eq!(metadata.receipt_version, Some(2));
    let requests = handle.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST "))
            .count(),
        1
    );
    assert!(requests[1].starts_with("GET /api/v1/application-runs/by-key/"));
}

#[tokio::test]
async fn native_outlier_metadata_preserves_full_u64_without_returning_invalid_output() {
    let (origin, handle) = server(|origin| {
        let (_, mut result) = pair(&fixture(origin));
        result.status = RunStatus::Failed;
        result.reason = Some(RunReason::InvalidOutput);
        result.result = None;
        result.result_delivery = ResultDelivery::None;
        let receipt = result.receipt.as_mut().unwrap();
        receipt.input_tokens = Some(u64::MAX - 10);
        receipt.total_tokens = Some(u64::MAX);
        result.validate(false).unwrap();
        vec![wire(&result)]
    });
    let connection = fixture(&origin);
    let client = ApplicationClient::new(&origin, Duration::from_secs(5)).unwrap();
    let (request, _) = pair(&connection);
    let metadata = client
        .run_by_key(
            &connection,
            &credential(&connection),
            &request.dispatch_key,
            &request.payload_digest,
        )
        .await
        .unwrap();
    assert_eq!(metadata.status, RunStatus::Failed);
    assert!(metadata.result.is_none());
    let receipt = metadata.receipt.unwrap();
    assert_eq!(receipt.total_tokens, Some(u64::MAX));
    assert_eq!(receipt.input_tokens, Some(u64::MAX - 10));
    assert_eq!(handle.join().unwrap().len(), 1);
}
