use super::*;
use serde_json::json;

fn user(text: &str) -> NativeItem {
    NativeItem::new(
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
    )
    .unwrap()
}
fn tool(name: &str) -> FunctionTool {
    FunctionTool::new(json!({"type":"function","name":name,"description":"PRIVATE_TOOL_DESCRIPTION",
        "strict":false,"parameters":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
          "properties":{"attributes":{"type":"object","additionalProperties":{"type":"string"}},
            "revision":{"type":"integer","format":"uint64","minimum":0,"maximum":18446744073709551615u64}},
          "required":["revision"]}})).unwrap()
}
fn input() -> NativeTurnInput {
    NativeTurnInput {
        instructions: "PRIVATE_INSTRUCTIONS: authorize each client effect separately".into(),
        history: vec![user("PRIVATE_REQUEST")],
        tools: vec![tool("mc_tool_0")],
        max_output_tokens: 2048,
    }
}
fn call(id: &str, name: &str) -> NativeItem {
    NativeItem::new(json!({"type":"function_call","id":format!("item_{id}"),"call_id":id,
        "name":name,"arguments":"{ \"revision\" : 7, \"attributes\": {\"context\": \"PRIVATE_ARGUMENT\"} }","status":"completed"})).unwrap()
}
fn outcome(id: &str) -> NativeItem {
    NativeItem::new(
        json!({"type":"function_call_output","call_id":id,"output":"PRIVATE_ACTUAL_TOOL_RESULT"}),
    )
    .unwrap()
}
fn reasoning(id: &str) -> NativeItem {
    NativeItem::new(json!({"type":"reasoning","id":id,"summary":[],
        "encrypted_content":"PRIVATE_OPAQUE_ENCRYPTED_STATE","status":null}))
    .unwrap()
}
fn assistant() -> NativeItem {
    NativeItem::new(json!({"type":"message","id":"message_1","role":"assistant","phase":"commentary","status":"completed",
        "content":[{"type":"output_text","text":"PRIVATE_ASSISTANT_TEXT","annotations":[],"logprobs":[]}]})).unwrap()
}
fn output(items: Vec<NativeItem>) -> NativeTurnOutput {
    NativeTurnOutput { output: items }
}

#[test]
fn native_input_keeps_real_function_schema_shapes_and_explicit_nonstrict_semantics() {
    let input = input();
    input.validate().unwrap();
    let bytes = serde_json::to_vec(&input).unwrap();
    let parsed = NativeTurnInput::parse(&bytes).unwrap();
    assert_eq!(parsed, input);
    let value = serde_json::to_value(parsed).unwrap();
    assert_eq!(value["tools"][0], input.tools[0].as_value().clone());
    assert_eq!(
        value["tools"][0]["parameters"]["properties"]["revision"]["maximum"].as_u64(),
        Some(u64::MAX)
    );
    assert_eq!(
        value["tools"][0]["parameters"]["properties"]["attributes"]["additionalProperties"],
        json!({"type":"string"})
    );
    assert_eq!(value["tools"][0]["strict"], false);
}

#[test]
fn native_turn_replays_every_original_item_before_actual_tool_outcomes() {
    let input = input();
    let output = output(vec![
        reasoning("reason_1"),
        assistant(),
        call("call_1", "mc_tool_0"),
    ]);
    output.validate_for(&input).unwrap();
    let actual = outcome("call_1");
    let next = output.continue_with(&input, vec![actual.clone()]).unwrap();
    assert_eq!(&next.history[1..4], &output.output);
    assert_eq!(next.history.last(), Some(&actual));
    assert_eq!(
        next.history[3].as_value()["arguments"],
        output.output[2].as_value()["arguments"]
    );
    assert_eq!(
        next.history[1].as_value()["encrypted_content"],
        "PRIVATE_OPAQUE_ENCRYPTED_STATE"
    );
    assert_eq!(next.history[0], input.history[0]);
    assert_eq!(next.tools, input.tools);
    NativeTurnInput::parse(&serde_json::to_vec(&next).unwrap()).unwrap();
}

#[test]
fn native_continuation_refuses_missing_repeated_or_orphan_effect_outcomes() {
    let input = input();
    let output = output(vec![
        call("call_1", "mc_tool_0"),
        call("call_2", "mc_tool_0"),
    ]);
    output.validate_for(&input).unwrap();
    for outcomes in [
        vec![],
        vec![outcome("call_1")],
        vec![outcome("call_1"), outcome("call_1")],
        vec![outcome("call_1"), outcome("unknown")],
        vec![user("invented permission")],
    ] {
        assert!(output.continue_with(&input, outcomes).is_err());
    }
    output
        .continue_with(&input, vec![outcome("call_2"), outcome("call_1")])
        .unwrap();
}

#[test]
fn native_history_does_not_regrant_tools_removed_from_the_current_catalog() {
    let first = input();
    let mut next = output(vec![call("call_1", "mc_tool_0")])
        .continue_with(&first, vec![outcome("call_1")])
        .unwrap();
    next.tools = vec![tool("mc_tool_1")];
    next.validate().unwrap();
    output(vec![call("call_2", "mc_tool_1")])
        .validate_for(&next)
        .unwrap();
    assert!(output(vec![call("call_2", "mc_tool_0")])
        .validate_for(&next)
        .is_err());
}

#[test]
fn native_output_rejects_cross_turn_or_duplicate_call_and_item_ids() {
    let first = input();
    let next = output(vec![reasoning("reason_1"), call("call_1", "mc_tool_0")])
        .continue_with(&first, vec![outcome("call_1")])
        .unwrap();
    for items in [
        vec![call("call_1", "mc_tool_0")],
        vec![call("call_2", "mc_tool_0"), call("call_2", "mc_tool_0")],
        vec![reasoning("reason_1"), call("call_2", "mc_tool_0")],
        vec![assistant(), assistant()],
    ] {
        assert!(output(items).validate_for(&next).is_err());
    }
    let mut duplicate = next.clone();
    duplicate.history.push(reasoning("reason_1"));
    assert!(duplicate.validate().is_err());
}

#[test]
fn native_input_requires_complete_history_and_denies_orphan_outcomes_and_role_escalation() {
    for items in [
        vec![outcome("call_1")],
        vec![call("call_1", "mc_tool_0")],
        vec![
            call("call_1", "mc_tool_0"),
            user("skip the pending effect"),
            outcome("call_1"),
        ],
    ] {
        let mut input = input();
        input.history = items;
        assert!(input.validate().is_err());
    }
    for role in ["developer", "system", "tool"] {
        assert!(NativeItem::new(json!({"type":"message","role":role,"content":[{"type":"input_text","text":"PRIVATE_ESCALATION"}]})).is_err());
    }
}

#[test]
fn native_function_calls_require_unambiguous_json_objects_without_argument_rewriting() {
    for arguments in [
        "[]",
        "null",
        "true",
        "{\"a\":1,\"a\":2}",
        "{\"a\":{\"b\":1,\"b\":2}}",
        "{} {}",
    ] {
        assert!(NativeItem::new(json!({"type":"function_call","call_id":"call_1","name":"mc_tool_0","arguments":arguments})).is_err());
    }
    let raw = "{ \"exact\":18446744073709551615, \"order\": [2,1] }";
    let call = NativeItem::new(
        json!({"type":"function_call","call_id":"call_1","name":"mc_tool_0","arguments":raw}),
    )
    .unwrap();
    assert_eq!(serde_json::to_value(call).unwrap()["arguments"], raw);
}

#[test]
fn native_wire_boundaries_reject_duplicate_fields_trailing_json_and_provider_state() {
    let input = input();
    let mut value = serde_json::to_value(&input).unwrap();
    value["previous_response_id"] = "PRIVATE_STORED_RESPONSE".into();
    assert!(NativeTurnInput::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    let bytes = serde_json::to_vec(&input).unwrap();
    let mut trailing = bytes.clone();
    trailing.extend(b"{}");
    assert!(NativeTurnInput::parse(&trailing).is_err());
    let duplicate = String::from_utf8(bytes).unwrap().replacen(
        "\"instructions\":",
        "\"instructions\":\"PRIVATE_DUPLICATE\",\"instructions\":",
        1,
    );
    let error = NativeTurnInput::parse(duplicate.as_bytes()).unwrap_err();
    assert!(!error.to_string().contains("PRIVATE_"));
    let duplicate_tool =
        r#"{"type":"function","name":"a","name":"b","strict":false,"parameters":{}}"#;
    assert!(serde_json::from_str::<FunctionTool>(duplicate_tool).is_err());
    let duplicate_item =
        r#"{"type":"function_call_output","call_id":"a","output":"x","output":"y"}"#;
    assert!(serde_json::from_str::<NativeItem>(duplicate_item).is_err());
}

#[test]
fn native_catalog_requires_explicit_schema_mode_and_excludes_hosted_tools() {
    let mut value = tool("mc_tool_0").as_value().clone();
    value.as_object_mut().unwrap().remove("strict");
    assert!(FunctionTool::new(value).is_err());
    for kind in [
        "web_search",
        "mcp",
        "code_interpreter",
        "computer",
        "custom",
    ] {
        assert!(FunctionTool::new(
            json!({"type":kind,"name":"mc_tool_0","strict":false,"parameters":{}})
        )
        .is_err());
    }
    let mut input = input();
    input.tools.push(input.tools[0].clone());
    assert!(input.validate().is_err());
}

#[test]
fn native_stateless_reasoning_requires_replayable_encrypted_state() {
    let original = reasoning("reason_1").as_value().clone();
    for mutation in [Value::Null, json!(""), json!(42)] {
        let mut value = original.clone();
        value["encrypted_content"] = mutation;
        assert!(NativeItem::new(value).is_err());
    }
    let mut value = original.clone();
    value.as_object_mut().unwrap().remove("encrypted_content");
    assert!(NativeItem::new(value).is_err());
    let parsed: NativeItem = serde_json::from_value(original.clone()).unwrap();
    assert_eq!(parsed.as_value(), &original);
}

#[test]
fn native_documented_direct_metadata_and_reasoning_content_are_preserved() {
    let input = input();
    let mut reason = reasoning("reason_1").as_value().clone();
    reason["content"] =
        json!([{"type":"reasoning_text","text":"PRIVATE_RETURNED_REASONING_CONTENT"}]);
    let mut called = call("call_1", "mc_tool_0").as_value().clone();
    called["async"] = false.into();
    called["caller"] = json!({"type":"direct"});
    called["namespace"] = Value::Null;
    let output = output(vec![
        NativeItem::new(reason.clone()).unwrap(),
        NativeItem::new(called.clone()).unwrap(),
    ]);
    output.validate_for(&input).unwrap();
    let mut actual = outcome("call_1").as_value().clone();
    actual["name"] = "mc_tool_0".into();
    actual["caller"] = json!({"type":"direct"});
    actual["namespace"] = Value::Null;
    actual["status"] = Value::Null;
    let next = output
        .continue_with(&input, vec![NativeItem::new(actual.clone()).unwrap()])
        .unwrap();
    let parsed = NativeTurnInput::parse(&serde_json::to_vec(&next).unwrap()).unwrap();
    assert_eq!(parsed.history[1].as_value(), &reason);
    assert_eq!(parsed.history[2].as_value(), &called);
    assert_eq!(parsed.history[3].as_value(), &actual);
    let mut null_content = reason;
    null_content["content"] = Value::Null;
    assert_eq!(
        NativeItem::new(null_content.clone()).unwrap().as_value(),
        &null_content
    );
}

#[test]
fn native_direct_mode_denies_programs_async_namespaces_and_misbound_named_outcomes() {
    for (key, value) in [
        ("async", json!(true)),
        ("namespace", json!("other_service")),
        ("caller", json!({"type":"program","caller_id":"program_1"})),
        (
            "caller",
            json!({"type":"direct","caller_id":"PRIVATE_EXTRA"}),
        ),
    ] {
        let mut called = call("call_1", "mc_tool_0").as_value().clone();
        called[key] = value;
        assert!(NativeItem::new(called).is_err());
    }
    let mut wrong = outcome("call_1").as_value().clone();
    wrong["name"] = "mc_tool_1".into();
    assert!(output(vec![call("call_1", "mc_tool_0")])
        .continue_with(&input(), vec![NativeItem::new(wrong).unwrap()])
        .is_err());
    let mut wrong_phase = assistant().as_value().clone();
    wrong_phase["phase"] = "PRIVATE_UNSUPPORTED_PHASE".into();
    assert!(NativeItem::new(wrong_phase).is_err());
}

#[test]
fn native_output_requires_an_answer_or_current_function_call_and_rejects_injected_outcomes() {
    let input = input();
    for items in [
        vec![],
        vec![reasoning("reason_1")],
        vec![outcome("call_1")],
        vec![user("provider user message")],
        vec![call("call_1", "unregistered")],
    ] {
        assert!(output(items).validate_for(&input).is_err());
    }
    output(vec![assistant()]).validate_for(&input).unwrap();
    let mut incomplete = call("call_1", "mc_tool_0").as_value().clone();
    incomplete["status"] = "in_progress".into();
    assert!(NativeItem::new(incomplete).is_err());
}

#[test]
fn native_parallel_call_limit_accepts_sixteen_without_restricting_the_workflow_to_one() {
    let input = input();
    let calls: Vec<_> = (0..MAX_TURN_CALLS)
        .map(|index| call(&format!("call_{index}"), "mc_tool_0"))
        .collect();
    let allowed = output(calls.clone());
    allowed.validate_for(&input).unwrap();
    allowed
        .continue_with(
            &input,
            (0..MAX_TURN_CALLS)
                .map(|index| outcome(&format!("call_{index}")))
                .collect(),
        )
        .unwrap();
    let mut too_many = calls;
    too_many.push(call("call_extra", "mc_tool_0"));
    assert!(output(too_many).validate_for(&input).is_err());
}

#[test]
fn native_size_depth_and_node_limits_are_bounded_before_provider_work() {
    let mut invalid_input = input();
    invalid_input.max_output_tokens = 0;
    assert!(invalid_input.validate().is_err());
    invalid_input = input();
    invalid_input.instructions = "x".repeat(MAX_INSTRUCTIONS_BYTES + 1);
    assert!(invalid_input.validate().is_err());
    invalid_input = input();
    invalid_input.history = (0..5).map(|_| user(&"x".repeat(MAX_TEXT_BYTES))).collect();
    assert_eq!(invalid_input.validate(), Err(ValidationError::TooLarge));
    let mut value = tool("mc_tool_0").as_value().clone();
    value["parameters"]["too_many_nodes"] = json!(vec![0; 16385]);
    assert_eq!(
        FunctionTool::new(value).unwrap_err(),
        ValidationError::TooLarge
    );
    let mut deep = Value::Null;
    for _ in 0..65 {
        deep = json!({"nested":deep});
    }
    let mut value = tool("mc_tool_0").as_value().clone();
    value["parameters"]["deep"] = deep;
    assert_eq!(
        FunctionTool::new(value).unwrap_err(),
        ValidationError::TooLarge
    );
    assert!(NativeTurnInput::parse(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
}

#[test]
fn native_large_aggregate_history_and_catalog_do_not_bypass_per_item_limits() {
    let mut input = input();
    input.tools = (0..20)
        .map(|index| {
            let mut value = tool(&format!("mc_tool_{index}")).as_value().clone();
            value["description"] = "x".repeat(16384).into();
            FunctionTool::new(value).unwrap()
        })
        .collect();
    assert_eq!(input.validate(), Err(ValidationError::TooLarge));
    input.tools = vec![tool("mc_tool_0")];
    let items = (0..6).map(|index| NativeItem::new(json!({"type":"message","id":format!("message_{index}"),"role":"assistant", "content":[{"type":"output_text","text":"x".repeat(MAX_TEXT_BYTES)}]})).unwrap()).collect();
    assert_eq!(
        output(items).validate_for(&input),
        Err(ValidationError::TooLarge)
    );
}

#[test]
fn native_output_parser_and_diagnostics_keep_private_content_out_of_errors_and_debug() {
    let input = input();
    let output = output(vec![reasoning("reason_1"), assistant()]);
    let bytes = serde_json::to_vec(&output).unwrap();
    assert_eq!(NativeTurnOutput::parse(&bytes, &input).unwrap(), output);
    let mut trailing = bytes;
    trailing.extend(b"{} PRIVATE_SENTINEL");
    let error = NativeTurnOutput::parse(&trailing, &input).unwrap_err();
    for debug in [
        format!("{input:?}"),
        format!("{output:?}"),
        format!("{:?}", input.tools),
        format!("{:?}", output.output),
        format!("{error:?}"),
        error.to_string(),
    ] {
        assert!(!debug.contains("PRIVATE_"));
    }
}
