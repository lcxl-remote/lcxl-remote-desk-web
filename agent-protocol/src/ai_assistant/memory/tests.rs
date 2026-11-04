use super::*;

#[test]
fn model_authored_writes_accept_content_without_extra_sources_and_reject_invalid_targets() {
    for operation in ["remember", "update"] {
        let mut value = serde_json::json!({"operation":operation,"content":"Call me yyy","context":"Preferred name"});
        if operation == "update" {
            value["memory_id"] = serde_json::json!("memory-1");
            value["expected_revision"] = serde_json::json!(2);
        }
        let request: MemoryWriteRequest = serde_json::from_value(value.clone()).unwrap();
        request.validate().unwrap();
        assert!(request.draft().unwrap().source_refs.is_empty());
        assert_eq!(request.draft().unwrap().content, "Call me yyy");
        value["content"] = serde_json::json!(" ");
        assert!(
            serde_json::from_value::<MemoryWriteRequest>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    for revision in [0, 1] {
        let request = MemoryWriteRequest::Forget {
            memory_id: String::new(),
            expected_revision: revision,
        };
        assert!(request.validate().is_err());
    }
}

#[test]
fn a_memory_write_device_selector_requires_device_scope() {
    let mut value = serde_json::json!({
        "operation":"remember","content":"Fixture fact","scope":"current_device","target_ref":"selected-device",
    });
    let request: MemoryWriteRequest = serde_json::from_value(value.clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(
        request.draft().unwrap().target_ref.as_deref(),
        Some("selected-device")
    );

    value["scope"] = serde_json::json!("personal");
    assert!(
        serde_json::from_value::<MemoryWriteRequest>(value.clone())
            .unwrap()
            .validate()
            .is_err()
    );
    value["scope"] = serde_json::json!("current_device");
    value["target_ref"] = serde_json::json!(" selected-device");
    assert!(
        serde_json::from_value::<MemoryWriteRequest>(value)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn successful_write_receipt_exposes_only_identity_and_version() {
    let result = MemoryWriteResult {
        outcome: MemoryWriteOutcome::Applied,
        memory: Some(MemoryWritten {
            memory_id: "memory-1".into(),
            revision: 2,
        }),
        reason: Some("committed".into()),
        control_receipt: None,
    };
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(
        value["memory"],
        serde_json::json!({"memory_id":"memory-1","revision":2})
    );
    assert_eq!(
        serde_json::from_value::<MemoryWriteResult>(value).unwrap(),
        result
    );
}

#[test]
fn draft_limit_errors_report_configured_utf8_counts_without_copying_text() {
    let mut limits = super::super::limits::AssistantMemoryLimits {
        draft_bytes: 8,
        ..Default::default()
    };
    for operation in ["remember", "update"] {
        let mut value = serde_json::json!({"operation":operation,"content":"中文","context":"🙂"});
        if operation == "update" {
            value["memory_id"] = serde_json::json!("memory-1");
            value["expected_revision"] = serde_json::json!(1);
        }
        let request: MemoryWriteRequest = serde_json::from_value(value).unwrap();
        let error = request.validate_with_limits(&limits).unwrap_err();
        assert_eq!(
            error,
            request
                .draft()
                .unwrap()
                .validate_with_limits(&limits)
                .unwrap_err()
        );
        for expected in [
            "$.content",
            "$.context",
            "maximum 8",
            "observed 10",
            "content=6",
            "context=4",
        ] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(!error.contains("中文"));
        assert!(!error.contains("🙂"));
        limits.draft_bytes = 10;
        request.validate_with_limits(&limits).unwrap();
        limits.draft_bytes = 8;
    }
    let request: MemoryWriteRequest = serde_json::from_value(
        serde_json::json!({"operation":"remember","content":"PRIVATE_MEMORY_VALUE"}),
    )
    .unwrap();
    assert!(
        !request
            .validate_with_limits(&limits)
            .unwrap_err()
            .contains("PRIVATE_MEMORY_VALUE")
    );
}

#[test]
fn source_limit_and_invalid_memory_field_errors_identify_their_fields() {
    let mut limits = super::super::limits::AssistantMemoryLimits {
        source_refs: 1,
        ..Default::default()
    };
    let request: MemoryWriteRequest = serde_json::from_value(
        serde_json::json!({"operation":"remember","content":"ok","source_refs":[
            {"kind":"input","id":"input-one","role":"evidence"},
            {"kind":"input","id":"input-two","role":"evidence"}
        ]}),
    )
    .unwrap();
    let error = request.validate_with_limits(&limits).unwrap_err();
    assert!(error.contains("$.source_refs"));
    assert!(error.contains("maximum 1"));
    assert!(error.contains("observed 2"));
    limits.source_refs = 2;
    request.validate_with_limits(&limits).unwrap();
    let invalid = MemoryWriteRequest::Forget {
        memory_id: "memory-one".into(),
        expected_revision: 0,
    };
    assert!(
        invalid
            .validate()
            .unwrap_err()
            .contains("$.expected_revision")
    );
}
