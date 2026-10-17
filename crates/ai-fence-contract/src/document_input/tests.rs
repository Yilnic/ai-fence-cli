use super::*;
fn limits() -> DocumentLimits {
    DocumentLimits {
        max_input_bytes: 1024,
        allowed_media_types: vec![
            DocumentMediaType::Pdf,
            DocumentMediaType::Png,
            DocumentMediaType::Jpeg,
        ],
    }
}
#[test]
fn prepared_documents_preserve_original_octets_exact_source_identity_and_redacted_debug() {
    for (kind, bytes) in [
        (
            DocumentMediaType::Pdf,
            b"%PDF-1.7\nPRIVATE_DOCUMENT_SENTINEL\n%%EOF".as_slice(),
        ),
        (
            DocumentMediaType::Png,
            b"\x89PNG\r\n\x1a\nPRIVATE_PNG_SENTINEL".as_slice(),
        ),
        (
            DocumentMediaType::Jpeg,
            b"\xff\xd8\xffPRIVATE_JPEG_SENTINEL\xff\xd9".as_slice(),
        ),
    ] {
        let prepared = PreparedDocument::prepare(bytes, kind, &limits()).unwrap();
        assert_eq!(prepared.as_bytes(), bytes);
        assert_eq!(prepared.source().source_bytes, bytes.len());
        assert_eq!(
            prepared.source().source_sha256,
            format!("{:x}", Sha256::digest(bytes))
        );
        assert!(prepared.matches(prepared.source()));
        assert!(!format!("{prepared:?} {:?}", prepared.source()).contains("PRIVATE"));
        let mut changed = prepared.source().clone();
        changed.source_sha256 = "b".repeat(64);
        assert!(!prepared.matches(&changed));
        changed = prepared.source().clone();
        changed.source_bytes += 1;
        assert!(!prepared.matches(&changed));
        changed = prepared.source().clone();
        changed.media_type = if kind == DocumentMediaType::Pdf {
            DocumentMediaType::Png
        } else {
            DocumentMediaType::Pdf
        };
        assert!(!prepared.matches(&changed));
        assert_eq!(prepared.into_bytes(), bytes);
    }
}
#[test]
fn document_limits_signatures_and_metadata_refuse_unknown_or_unbounded_source_forms() {
    let bytes = b"%PDF-1.7\nPRIVATE_DOCUMENT_SENTINEL\n%%EOF";
    assert!(PreparedDocument::prepare(bytes, DocumentMediaType::Png, &limits()).is_err());
    assert!(PreparedDocument::prepare(
        b"https://evil.test/file",
        DocumentMediaType::Pdf,
        &limits()
    )
    .is_err());
    assert!(PreparedDocument::prepare(b"%PDF-", DocumentMediaType::Pdf, &limits()).is_err());
    let mut small = limits();
    small.max_input_bytes = 12;
    assert!(PreparedDocument::prepare(bytes, DocumentMediaType::Pdf, &small).is_err());
    for bad in [0, 11, MAX_DOCUMENT_BYTES + 1] {
        let mut l = limits();
        l.max_input_bytes = bad;
        assert!(!l.valid());
    }
    let mut duplicate = limits();
    duplicate.allowed_media_types.push(DocumentMediaType::Pdf);
    assert!(!duplicate.valid());
    let mut empty = limits();
    empty.allowed_media_types.clear();
    assert!(!empty.valid());
    let prepared = PreparedDocument::prepare(bytes, DocumentMediaType::Pdf, &limits()).unwrap();
    let wire = serde_json::to_value(prepared.source()).unwrap();
    for name in ["filename", "file_id", "url", "file_data"] {
        let mut bad = wire.clone();
        bad[name] = serde_json::json!("PRIVATE_SENTINEL");
        assert!(serde_json::from_value::<DocumentSource>(bad).is_err());
    }
    let mut bad = wire;
    bad["media_type"] = serde_json::json!("text/plain");
    assert!(serde_json::from_value::<DocumentSource>(bad).is_err());
}
