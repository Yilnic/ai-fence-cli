use super::*;
use serde_json::json;
fn value() -> WorkloadOffer {
    serde_json::from_value(json!({"schema_version":1,"binding":{"binding_id":"binding","application_id":"app","stage_id":"stage","service_name":"ai"},
        "binding_revision":2,"workload_role":"review","profile_id":"profile","profile_revision":3,
        "input_kind":"structured_text","provider_origin":"https://provider.example.test","selected_model":"provider-original",
        "data_policy":{"url":"https://provider.example.test/privacy","revision":"review-4","source":"operator_configured"},
        "gateway_retention":"metadata_only","max_request_bytes":8192,"max_input_tokens":16384,"max_output_tokens":2048,
        "documents":null,"speech":null,"pricing":{"unit":"tokens","snapshot":{"schema_version":2,"input":2,"cached_input":1,"cache_write":3,"output":4,
          "tiers":[{"threshold_tokens":"18446744073709551615","input":5,"cached_input":null,"cache_write":7,"output":8,"marginal":true}]}},
        "maximum_estimated_call_usd":0.15,"enforced_capabilities":["application_runs_v1","full_run_receipts_v2","metadata_only_retention_v1","application_workload_offer_v1"]})).unwrap()
}
fn caps(offer: &mut WorkloadOffer) {
    offer.enforced_capabilities = offer
        .protocol_capabilities()
        .into_iter()
        .chain([WORKLOAD_OFFER_V1])
        .map(str::to_owned)
        .collect();
}
#[test]
fn workload_offer_preserves_original_target_policy_complete_tiers_and_all_modes() {
    let text = value();
    text.validate().unwrap();
    let mut document = text.clone();
    document.input_kind = InputKind::StructuredDocument;
    document.documents = Some(DocumentLimits {
        max_input_bytes: 16384,
        allowed_media_types: vec![crate::document_input::DocumentMediaType::Pdf],
    });
    caps(&mut document);
    document.validate().unwrap();
    let mut native = text.clone();
    native.input_kind = InputKind::NativeFunctions;
    caps(&mut native);
    native.validate().unwrap();
    let mut speech = text.clone();
    speech.input_kind = InputKind::AudioTranscription;
    speech.max_input_tokens = 0;
    speech.max_output_tokens = 0;
    speech.speech = Some(SpeechOffer {
        limits: AudioLimits {
            max_input_bytes: 32768,
            max_input_duration_ms: 2000,
        },
        segment_timestamps: true,
    });
    speech.pricing = OfferPricing::Duration {
        snapshot: DurationPricing {
            schema_version: 1,
            usd_per_minute: 0.06,
            billing_quantum_ms: 1000,
            minimum_billable_ms: 1000,
        },
    };
    caps(&mut speech);
    speech.validate().unwrap();
    for original in [text, document, native, speech] {
        let wire = serde_json::to_value(&original).unwrap();
        let restored: WorkloadOffer = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored, original);
        assert_eq!(wire["selected_model"], "provider-original");
        assert_eq!(wire["data_policy"]["revision"], "review-4");
        if original.input_kind != InputKind::AudioTranscription {
            assert_eq!(
                wire["pricing"]["snapshot"]["tiers"][0]["threshold_tokens"],
                u64::MAX.to_string()
            );
        }
    }
}
#[test]
fn workload_offer_rejects_unsafe_urls_unproven_modes_missing_rates_and_ambiguous_wire() {
    let original = value();
    for case in 0..22 {
        let mut bad = original.clone();
        match case {
            0 => bad.schema_version = 2,
            1 => bad.binding_revision = 0,
            2 => bad.profile_revision = 0,
            3 => bad.workload_role = "../profiles".into(),
            4 => bad.provider_origin = "https://provider.example.test/private/path".into(),
            5 => bad.provider_origin = "https://secret@provider.example.test".into(),
            6 => bad.provider_origin = "http://external.example.test".into(),
            7 => bad.data_policy.url = "https://provider.example.test/policy?private=secret".into(),
            8 => bad.data_policy.url = "javascript:payload".into(),
            9 => bad.maximum_estimated_call_usd = f64::NAN,
            10 => bad.maximum_estimated_call_usd = -1.0,
            11 => bad.enforced_capabilities.retain(|v| v != METADATA_ONLY_V1),
            12 => bad.enforced_capabilities.push(METADATA_ONLY_V1.into()),
            13 => bad.max_input_tokens = 1,
            14 => bad.input_kind = InputKind::StructuredDocument,
            15 => bad.input_kind = InputKind::AudioTranscription,
            16 => {
                if let OfferPricing::Tokens { snapshot } = &mut bad.pricing {
                    snapshot.schema_version = 1;
                }
            }
            17 => {
                if let OfferPricing::Tokens { snapshot } = &mut bad.pricing {
                    snapshot.tiers[0].threshold_tokens = "018446744073709551615".into();
                }
            }
            19 => bad
                .enforced_capabilities
                .push(crate::application_native::NATIVE_FUNCTION_TURNS_V1.into()),
            20 => bad
                .enforced_capabilities
                .push(DURATION_RUN_RECEIPTS_V3.into()),
            21 => bad.data_policy.url = "\nhttps://provider.example.test/privacy".into(),
            _ => {
                if let OfferPricing::Tokens { snapshot } = &mut bad.pricing {
                    snapshot.cache_write = f64::INFINITY;
                }
            }
        }
        assert!(bad.validate().is_err(), "case {case}");
    }
    let mut wire = serde_json::to_value(original).unwrap();
    wire["pricing"]["snapshot"]
        .as_object_mut()
        .unwrap()
        .remove("cache_write");
    assert!(serde_json::from_value::<WorkloadOffer>(wire).is_err());
    let wire = serde_json::to_string(&value()).unwrap();
    assert!(serde_json::from_str::<WorkloadOffer>(&wire.replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1
    ))
    .is_err());
    let mut wire = serde_json::to_value(value()).unwrap();
    wire["credential"] = "must-not-be-in-disclosure".into();
    assert!(serde_json::from_value::<WorkloadOffer>(wire).is_err());
}
#[test]
fn workload_offer_matching_keeps_exact_binding_role_and_profile_revisions() {
    use crate::application::{ProfileReference, ServiceAuthentication};
    let offer = value();
    let mut connection = ManagedConnection {
        schema_version: 1,
        fence_url: "https://fence.example.test".into(),
        binding: offer.binding.clone(),
        binding_revision: 2,
        auth: ServiceAuthentication {
            issuer: "https://id.example.test".into(),
            token_endpoint: "https://id.example.test/token".into(),
            client_id: "client".into(),
            audience: "app".into(),
            scope: "app.ai".into(),
            client_secret_ref: "secret:app.ai".into(),
        },
        profiles: std::collections::BTreeMap::from([(
            "review".into(),
            ProfileReference {
                id: "profile".into(),
                revision: None,
            },
        )]),
    };
    assert!(offer.matches(&connection, "review", 3));
    assert!(!offer.matches(&connection, "review", 4));
    assert!(!offer.matches(&connection, "other", 3));
    connection.binding.stage_id = "foreign-stage".into();
    assert!(!offer.matches(&connection, "review", 3));
    connection.binding = offer.binding.clone();
    connection.profiles.get_mut("review").unwrap().revision = Some(2);
    assert!(!offer.matches(&connection, "review", 3));
}
