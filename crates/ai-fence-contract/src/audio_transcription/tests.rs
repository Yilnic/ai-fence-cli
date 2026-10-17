use super::*;
use serde_json::json;

fn response(value: Value, required: bool) -> TranscriptionObservation {
    parse_response(&serde_json::to_vec(&value).unwrap(), 2000, required).unwrap()
}

fn valid() -> Value {
    json!({"text":"PRIVATE_TRANSCRIPT", "language":"german", "duration":2,
        "segments":[{"id":0,"start":0.125,"end":1.9999,"text":"PRIVATE_SEGMENT"}],
        "usage":{"type":"duration","seconds":3}})
}

#[test]
fn measured_source_and_reported_billing_duration_are_distinct() {
    let parsed = response(valid(), true);
    assert_eq!(parsed.reported_duration_ms, Some(3000));
    let output = parsed.output.unwrap();
    assert_eq!(output.source_duration_ms, 2000);
    assert_eq!(output.text, "PRIVATE_TRANSCRIPT");
    let segment = &output.segments.as_ref().unwrap()[0];
    assert_eq!(segment.start, 0.125);
    assert_eq!(segment.end, 1.9999);
    assert_eq!(segment.text, "PRIVATE_SEGMENT");
    output.validate_for(2000, true).unwrap();
    assert!(output.validate_for(1999, true).is_err());
    assert_eq!(
        serde_json::from_value::<TranscriptionOutput>(json!(output)).unwrap(),
        output
    );
}

#[test]
fn reported_usage_survives_missing_wrong_or_errored_transcript() {
    for value in [
        json!({"usage":{"type":"duration","seconds":3}}),
        json!({"text":42,"usage":{"type":"duration","seconds":3}}),
        json!({"text":"PRIVATE_TRANSCRIPT","error":{"message":"PRIVATE_ERROR"},"usage":{"type":"duration","seconds":3}}),
        json!({"text":"PRIVATE_TRANSCRIPT","task":"translate","usage":{"type":"duration","seconds":3}}),
        json!({"text":"PRIVATE_TRANSCRIPT","language":42,"usage":{"type":"duration","seconds":3}}),
    ] {
        let parsed = response(value, false);
        assert!(parsed.output.is_err());
        assert_eq!(parsed.reported_duration_ms, Some(3000));
    }
}

#[test]
fn invalid_timestamp_order_type_and_source_bounds_reject_without_losing_usage() {
    for segments in [
        json!([{"start":-0.001,"end":2,"text":"PRIVATE_SEGMENT"}]),
        json!([{"start":1,"end":0.9,"text":"PRIVATE_SEGMENT"}]),
        json!([{"start":0,"end":2.001,"text":"PRIVATE_SEGMENT"}]),
        json!([{"start":0,"end":"PRIVATE_BAD_TIME","text":"PRIVATE_SEGMENT"}]),
        json!([{"start":0,"text":"PRIVATE_SEGMENT"}]),
        json!([{"start":1,"end":2,"text":"a"},{"start":0,"end":1,"text":"b"}]),
        json!([{"start":0,"end":2,"text":null}]),
    ] {
        let mut value = valid();
        value["segments"] = segments;
        let parsed = response(value, true);
        assert!(parsed.output.is_err());
        assert_eq!(parsed.reported_duration_ms, Some(3000));
    }
}

#[test]
fn missing_requested_segments_are_unknown_while_explicit_silence_is_valid() {
    let mut value = valid();
    value.as_object_mut().unwrap().remove("segments");
    assert!(response(value.clone(), false).output.is_ok());
    assert_eq!(
        response(value, true).output.unwrap_err(),
        TranscriptionError::MissingSegments
    );
    let mut value = valid();
    value["segments"] = json!([]);
    assert!(response(value.clone(), true).output.is_err());
    value["text"] = json!("");
    assert!(response(value, true).output.is_ok());
}

#[test]
fn unknown_or_mixed_usage_is_not_inferred_from_output_duration() {
    for usage in [
        Value::Null,
        json!({}),
        json!({"type":"tokens","input_tokens":1,"output_tokens":2,"total_tokens":3}),
        json!({"type":"duration","seconds":2,"total_tokens":0}),
        json!({"type":"duration","seconds":2,"extra":"PRIVATE_USAGE"}),
        json!({"type":"duration","seconds":"2"}),
        json!({"type":"duration","seconds":0}),
        json!({"type":"duration","seconds":-1}),
    ] {
        let mut value = valid();
        value["usage"] = usage;
        let parsed = response(value, true);
        assert!(parsed.output.is_ok());
        assert_eq!(parsed.reported_duration_ms, None);
    }
}

#[test]
fn provider_duration_preserves_outliers_and_checked_integer_units() {
    assert_eq!(
        reported_duration_ms(&json!({"type":"duration","seconds":900.001})),
        Some(900001)
    );
    let seconds = u64::MAX / 1000;
    assert_eq!(
        reported_duration_ms(&json!({"type":"duration","seconds":seconds})),
        Some(seconds * 1000)
    );
    assert_eq!(
        reported_duration_ms(&json!({"type":"duration","seconds":seconds + 1})),
        None
    );
    assert_eq!(
        reported_duration_ms(&json!({"type":"duration","seconds":1e300})),
        None
    );
    assert_eq!(
        reported_duration_ms(&json!({"type":"duration","seconds":0.00001})),
        Some(1)
    );
}

#[test]
fn ambiguous_json_is_rejected_even_in_ignored_provider_fields() {
    for bytes in [
        br#"{"text":"PRIVATE_FIRST","text":"PRIVATE_SECOND"}"#.as_slice(),
        br#"{"text":"PRIVATE_TRANSCRIPT","usage":{"type":"duration","seconds":3,"seconds":2}}"#,
        br#"{"text":"PRIVATE_TRANSCRIPT","ignored":{"key":1,"\u006bey":2}}"#,
        br#"{"text":"PRIVATE_TRANSCRIPT"} {}"#,
        b"[]",
    ] {
        let error = parse_response(bytes, 2000, false).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("PRIVATE_"));
    }
}

#[test]
fn response_byte_depth_node_and_segment_limits_are_enforced() {
    assert!(parse_response(&vec![b'x'; MAX_RESPONSE_BYTES + 1], 2000, false).is_err());
    let mut value = Value::Null;
    for _ in 0..MAX_JSON_DEPTH + 1 {
        value = json!([value]);
    }
    assert!(parse_response(&serde_json::to_vec(&value).unwrap(), 2000, false).is_err());
    let values = json!({"text":"ok","ignored":vec![0; MAX_JSON_NODES]});
    assert!(parse_response(&serde_json::to_vec(&values).unwrap(), 2000, false).is_err());
    let mut value = valid();
    value["segments"] = json!(vec![
        json!({"start":0,"end":1,"text":"a"});
        MAX_SEGMENTS + 1
    ]);
    assert_eq!(
        response(value, true).output.unwrap_err(),
        TranscriptionError::TooLarge
    );
}

#[test]
fn output_debug_and_errors_never_retain_private_content() {
    let parsed = response(valid(), true);
    assert!(!format!("{parsed:?}").contains("PRIVATE_"));
    let output = parsed.output.unwrap();
    assert!(!format!("{output:?} {:?}", output.segments).contains("PRIVATE_"));
    let mut invalid = output.clone();
    invalid.segments.as_mut().unwrap()[0].end = f64::INFINITY;
    assert!(!invalid
        .validate_for(2000, true)
        .unwrap_err()
        .to_string()
        .contains("PRIVATE_"));
}

fn wav_fixture() -> Vec<u8> {
    let mut wav =
        b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data"
            .to_vec();
    wav.extend_from_slice(&64000u32.to_le_bytes());
    wav.extend_from_slice(&vec![0u8; 64000]);
    let length = wav.len() as u32 - 8;
    wav[4..8].copy_from_slice(&length.to_le_bytes());
    wav
}
#[test]
fn prepared_wave_hashes_canonical_audio_and_binds_measured_source() {
    let limits = AudioLimits {
        max_input_bytes: MAX_AUDIO_BYTES,
        max_input_duration_ms: MAX_INPUT_DURATION_MS,
    };
    let source = wav_fixture();
    let prepared = PreparedAudio::prepare(&source, &limits).unwrap();
    assert_eq!(prepared.as_bytes(), source);
    assert_eq!(prepared.duration_ms(), 2000);
    let mut metadata = source[..36].to_vec();
    metadata.extend_from_slice(b"LIST\x10\0\0\0PRIVATE_METADATA");
    metadata.extend_from_slice(&source[36..]);
    let length = metadata.len() as u32 - 8;
    metadata[4..8].copy_from_slice(&length.to_le_bytes());
    let stripped = PreparedAudio::prepare(&metadata, &limits).unwrap();
    assert_eq!(stripped.sha256(), prepared.sha256());
    assert_eq!(stripped.as_bytes(), source);
    let input = TranscriptionInput {
        audio_sha256: prepared.sha256().into(),
        source_duration_ms: 2000,
        language: None,
        segment_timestamps: true,
    };
    assert!(prepared.matches(&input));
    let mut changed = source.clone();
    changed[44] = 1;
    assert!(!PreparedAudio::prepare(&changed, &limits)
        .unwrap()
        .matches(&input));
    let mut wrong = input.clone();
    wrong.source_duration_ms = 1;
    assert!(!prepared.matches(&wrong));
    assert!(!format!("{stripped:?} {input:?}").contains("PRIVATE_"));
    for (offset, value) in [(0, b'X'), (20, 3), (22, 0), (28, 1), (32, 3), (34, 8)] {
        let mut bad = source.clone();
        bad[offset] = value;
        assert!(PreparedAudio::prepare(&bad, &limits).is_err());
    }
    let short = AudioLimits {
        max_input_duration_ms: 1999,
        ..limits
    };
    assert!(PreparedAudio::prepare(&source, &short).is_err());
}

#[test]
fn wave_data_chunk_is_recorded_before_format_and_duplicates_are_rejected() {
    let limits = AudioLimits {
        max_input_bytes: MAX_AUDIO_BYTES,
        max_input_duration_ms: MAX_INPUT_DURATION_MS,
    };
    let source = wav_fixture();
    let mut data_first = source[..12].to_vec();
    data_first.extend_from_slice(&source[36..]);
    data_first.extend_from_slice(&source[12..36]);
    let prepared = PreparedAudio::prepare(&data_first, &limits).unwrap();
    assert_eq!(prepared.as_bytes(), source);
    assert_eq!(prepared.duration_ms(), 2000);

    let mut empty_data = b"data".to_vec();
    empty_data.extend_from_slice(&0u32.to_le_bytes());
    for extra in [source[36..].to_vec(), empty_data] {
        let mut duplicate = source.clone();
        duplicate.extend_from_slice(&extra);
        let length = duplicate.len() as u32 - 8;
        duplicate[4..8].copy_from_slice(&length.to_le_bytes());
        assert!(matches!(
            PreparedAudio::prepare(&duplicate, &limits),
            Err(TranscriptionError::InvalidAudio)
        ));
    }
}
