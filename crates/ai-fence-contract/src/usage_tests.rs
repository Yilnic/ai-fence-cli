use super::*;

#[test]
fn legacy_usage_keeps_its_wire_shape_and_duration_stays_separate_from_tokens() {
    let old = serde_json::json!({
        "input_tokens": 3, "output_tokens": 4, "cache_read_tokens": null,
        "cache_write_tokens": null, "total_tokens": 7, "cost_estimate": 0.01
    });
    let usage: GatewayUsage = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(usage.input_audio_ms, None);
    assert_eq!(serde_json::to_value(usage).unwrap(), old);
    let speech = GatewayUsage {
        input_audio_ms: Some(2000),
        cost_estimate: Some(0.02),
        ..Default::default()
    };
    let wire = serde_json::to_value(&speech).unwrap();
    assert_eq!(wire["input_audio_ms"], 2000);
    assert!(wire["input_tokens"].is_null());
    assert!(wire["total_tokens"].is_null());
    assert_eq!(
        serde_json::from_value::<GatewayUsage>(wire).unwrap(),
        speech
    );
}

#[test]
fn transcription_endpoint_has_an_explicit_stable_wire_and_storage_name() {
    let endpoint = ProviderEndpointType::OpenAIAudioTranscriptions;
    assert_eq!(endpoint.as_str(), "openai_audio_transcriptions");
    assert_eq!(
        serde_json::to_value(endpoint).unwrap(),
        "openai_audio_transcriptions"
    );
    assert_eq!(
        serde_json::from_value::<ProviderEndpointType>(serde_json::json!(endpoint.as_str()))
            .unwrap(),
        endpoint
    );
}
