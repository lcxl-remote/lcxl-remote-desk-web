use super::*;
use read::{ReadMode, ReadRequest, read_page};

fn metadata(id: &str, kind: ContentKind, content: &str) -> AttachmentMetadata {
    AttachmentMetadata {
        attachment_id: id.into(),
        conversation_id: "conversation".into(),
        actor_id: "owner".into(),
        device_id: "device".into(),
        message_id: "message".into(),
        tool_call_id: "call".into(),
        part: "body".into(),
        kind,
        media_type: "text/plain".into(),
        original_bytes: content.len() as u64,
        size_bytes: content.len() as u64,
        original_sha256: digest(content.as_bytes()),
        sha256: digest(content.as_bytes()),
        source_truncated: false,
        storage_truncated: false,
        created_at_unix_ms: 1,
        last_accessed_at_unix_ms: 1,
        availability: Availability::Available,
        image_source: None,
        source_envelope: None,
    }
}

fn request() -> ReadRequest {
    ReadRequest {
        attachment_id: "a".into(),
        selection: ReadMode::Read {
            start_line: None,
            end_line: None,
        },
        limit: 100,
        max_bytes: MAX_PAGE_BYTES,
        cursor: None,
    }
}

#[test]
fn result_thresholds_do_not_mutate_json() {
    for size in [
        INLINE_BYTES - 1,
        INLINE_BYTES,
        INLINE_BYTES + 1,
        MAX_JSON_BYTES,
    ] {
        let content = format!("\"{}\"", "x".repeat(size - 2));
        let prepared = prepare_text(ContentKind::Json, content.clone()).unwrap();
        assert_eq!(prepared.content, content);
        assert_eq!(prepared.external, size > INLINE_BYTES);
    }
    let error = prepare_text(
        ContentKind::Json,
        format!("\"{}\"", "x".repeat(MAX_JSON_BYTES - 1)),
    )
    .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::OutputLimitExceeded);
    assert!(!error.retryable);
    assert!(prepare_text(ContentKind::Json, "{broken".into()).is_err());
}

#[test]
fn text_prefix_preserves_utf8_and_original_digest() {
    let input = "界".repeat(140_000);
    let prepared = prepare_text(ContentKind::Text, input.clone()).unwrap();
    assert!(prepared.storage_truncated);
    assert_eq!(prepared.original_bytes, input.len());
    assert!(prepared.content.len() <= MAX_TEXT_BYTES);
    assert_eq!(prepared.content.len(), 399_999);
    assert_eq!(prepared.original_sha256, digest(input.as_bytes()));
    assert_ne!(prepared.sha256, prepared.original_sha256);
}

#[test]
fn json_cannot_be_searched_and_tampering_is_rejected() {
    let content = "{\"name\":\"保存\"}";
    let meta = metadata("a", ContentKind::Json, content);
    let mut req = request();
    req.selection = ReadMode::Search {
        queries: vec!["保存".into()],
        ignore_case: false,
        before_context: 0,
        after_context: 0,
    };
    assert!(read_page(&meta, content.as_bytes(), &req).is_err());
    req.selection = ReadMode::Read {
        start_line: None,
        end_line: None,
    };
    assert!(read_page(&meta, b"{}", &req).is_err());
}

#[test]
fn json_page_marks_fragments_even_when_a_fragment_is_itself_valid_json() {
    let content = "[\n1234\n]";
    let meta = metadata("a", ContentKind::Json, content);
    let mut req = request();
    assert!(
        !read_page(&meta, content.as_bytes(), &req)
            .unwrap()
            .json_fragment
    );
    req.selection = ReadMode::Read {
        start_line: Some(2),
        end_line: Some(2),
    };
    let page = read_page(&meta, content.as_bytes(), &req).unwrap();
    assert!(page.json_fragment);
    assert!(!page.has_more);
    assert!(serde_json::from_str::<serde_json::Value>(&page.lines[0].text).is_ok());
}

#[test]
fn long_line_pages_reconstruct_original_without_splitting_characters() {
    let content = format!("{}\r\nend\n", "界\\\"".repeat(9000));
    let meta = metadata("a", ContentKind::Text, &content);
    let mut req = request();
    req.max_bytes = 101;
    let mut reconstructed = String::new();
    for _ in 0..1000 {
        let page = read_page(&meta, content.as_bytes(), &req).unwrap();
        assert!(page.body_bytes <= 101);
        assert!(!page.lines.is_empty());
        for line in &page.lines {
            reconstructed.push_str(&line.text);
        }
        req.cursor = page.cursor;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(reconstructed, content);
}

#[test]
fn search_merges_context_preserves_crlf_and_binds_cursor() {
    let content = "before\r\nSAVE first\r\n保存 second\r\nafter\r\n";
    let meta = metadata("a", ContentKind::Text, content);
    let mut req = request();
    req.selection = ReadMode::Search {
        queries: vec!["save".into(), "保存".into()],
        ignore_case: true,
        before_context: 1,
        after_context: 1,
    };
    req.limit = 1;
    let page = read_page(&meta, content.as_bytes(), &req).unwrap();
    assert_eq!(
        page.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(page.queries, vec!["save", "保存"]);
    assert_eq!(page.lines[1].matched_queries, vec![0]);
    req.cursor = page.cursor;
    let next = read_page(&meta, content.as_bytes(), &req).unwrap();
    assert_eq!(
        next.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(!next.has_more);
    req.selection = ReadMode::Search {
        queries: vec!["different".into()],
        ignore_case: false,
        before_context: 0,
        after_context: 0,
    };
    assert!(read_page(&meta, content.as_bytes(), &req).is_err());
}

#[test]
fn evicted_is_not_an_empty_search_and_no_hit_is_success() {
    let mut meta = metadata("a", ContentKind::Text, "hello");
    let mut req = request();
    req.selection = ReadMode::Search {
        queries: vec!["missing".into()],
        ignore_case: false,
        before_context: 0,
        after_context: 0,
    };
    assert!(read_page(&meta, b"hello", &req).unwrap().lines.is_empty());
    meta.availability = Availability::Evicted { at_unix_ms: 7 };
    assert!(
        read_page(&meta, b"hello", &req)
            .unwrap_err()
            .message
            .contains("evicted")
    );
}

#[test]
fn quota_uses_cross_type_lru_and_checks_batch_before_eviction() {
    let mut rows = vec![
        metadata("a", ContentKind::Text, ""),
        metadata("b", ContentKind::Image, ""),
        metadata("c", ContentKind::Json, ""),
    ];
    for row in &mut rows {
        row.size_bytes = MAX_SESSION_BYTES / 3;
    }
    rows[0].last_accessed_at_unix_ms = 100;
    rows[1].last_accessed_at_unix_ms = 3;
    rows[2].last_accessed_at_unix_ms = 2;
    assert!(eviction_candidates(&rows, 0).unwrap().is_empty());
    assert_eq!(eviction_candidates(&rows, 2).unwrap()[0].attachment_id, "c");
    assert!(eviction_candidates(&rows, MAX_SESSION_BYTES + 1).is_err());
    rows[2].availability = Availability::Deleted { at_unix_ms: 4 };
    assert!(eviction_candidates(&rows, 2).unwrap().is_empty());
    rows[1].conversation_id = "other".into();
    assert!(eviction_candidates(&rows, 1).is_err());
}

fn delivery(
    parts: Vec<batch::OutputPart>,
    time: u64,
) -> Result<batch::PreparedDelivery, AgentError> {
    batch::prepare_delivery(
        &batch::DeliveryIdentity {
            conversation_id: "conversation",
            actor_id: "owner",
            device_id: "device",
            message_id: "message",
            tool_call_id: "call",
        },
        parts,
        time,
    )
}

fn text_part(name: &str, count: usize) -> batch::OutputPart {
    batch::OutputPart {
        name: name.into(),
        content: batch::PartContent::Text("x".repeat(count)),
        source_truncated: false,
    }
}

#[test]
fn streams_are_externalized_individually_without_envelope_recursion() {
    let small = delivery(
        vec![text_part("stdout", 4096), text_part("stderr", 4096)],
        1,
    )
    .unwrap();
    assert!(small.attachments.is_empty());
    assert_eq!(small.parts.len(), 2);
    assert!(serde_json::to_vec(&small.parts).unwrap().len() > INLINE_BYTES);
    let split = delivery(
        vec![text_part("stdout", 4097), text_part("stderr", 4096)],
        1,
    )
    .unwrap();
    assert_eq!(split.attachments.len(), 1);
    assert_eq!(split.attachments[0].metadata.part, "stdout");
    assert_eq!(split.stored_bytes, 4097);
    let reference = serde_json::to_string(&split.parts[0]).unwrap();
    assert!(!reference.contains(&"x".repeat(20)));
    assert!(!reference.contains("preview"));
}

#[test]
fn batch_rejects_late_invalid_json_or_image_without_partial_delivery() {
    let mut parts = vec![text_part("stdout", 5000)];
    parts.push(batch::OutputPart {
        name: "body".into(),
        content: batch::PartContent::Json("{broken".into()),
        source_truncated: false,
    });
    assert!(delivery(parts, 1).is_err());
    assert!(
        delivery(
            vec![
                text_part("stdout", 5000),
                batch::OutputPart {
                    name: "image".into(),
                    content: batch::PartContent::ImageDataUrl(
                        "data:image/png;base64,notvalid!".into()
                    ),
                    source_truncated: false,
                }
            ],
            1
        )
        .is_err()
    );
    assert!(delivery(vec![text_part("same", 5000), text_part("same", 5000)], 1).is_err());
}

#[test]
fn source_declared_text_is_not_reclassified_as_json() {
    let parts = vec![batch::OutputPart {
        name: "stdout".into(),
        content: batch::PartContent::Text(format!("{{{}", "x".repeat(40_000))),
        source_truncated: true,
    }];
    let prepared = delivery(parts, 1).unwrap();
    assert_eq!(prepared.attachments[0].metadata.kind, ContentKind::Text);
    assert!(prepared.parts[0].source_truncated);
    assert!(!prepared.attachments[0].metadata.storage_truncated);
}

#[test]
fn batch_retries_preserve_access_time_and_never_resurrect_tombstones() {
    let first = delivery(vec![text_part("stdout", 5000)], 1).unwrap();
    let retry = delivery(vec![text_part("stdout", 5000)], 99).unwrap();
    let mut stored = first.attachments[0].metadata.clone();
    stored.last_accessed_at_unix_ms = 20;
    let plan = batch::plan_batch(&[stored.clone()], &retry.attachments).unwrap();
    assert!(plan.insert_indices.is_empty());
    assert!(plan.evict_ids.is_empty());
    let changed = delivery(vec![text_part("stdout", 5001)], 99).unwrap();
    assert!(batch::plan_batch(&[stored.clone()], &changed.attachments).is_err());
    stored.availability = Availability::Evicted { at_unix_ms: 21 };
    assert!(batch::plan_batch(&[stored.clone()], &retry.attachments).is_err());
    stored.availability = Availability::Deleted { at_unix_ms: 21 };
    assert!(batch::plan_batch(&[stored], &retry.attachments).is_err());
}

#[test]
fn batch_quota_protects_already_present_parts_from_its_own_eviction() {
    let incoming = delivery(
        vec![text_part("stdout", 5000), text_part("stderr", 5000)],
        1,
    )
    .unwrap();
    let protected = incoming.attachments[0].metadata.clone();
    let mut other = metadata("other", ContentKind::Text, "");
    other.size_bytes = MAX_SESSION_BYTES - 5000;
    other.last_accessed_at_unix_ms = 100;
    let plan = batch::plan_batch(&[protected, other], &incoming.attachments).unwrap();
    assert_eq!(plan.insert_indices, vec![1]);
    assert_eq!(plan.evict_ids, vec!["other"]);
    let mut cross_subject = incoming.attachments.clone();
    cross_subject[1].metadata.actor_id = "someone_else".into();
    assert!(batch::plan_batch(&[], &cross_subject).is_err());
}

#[test]
fn many_short_lines_keep_metadata_bounded_and_can_be_fully_read() {
    let content = "x\n".repeat(2000);
    let meta = metadata("a", ContentKind::Text, &content);
    let mut req = request();
    req.limit = 1000;
    let mut recovered = String::new();
    loop {
        let page = read_page(&meta, content.as_bytes(), &req).unwrap();
        assert!(!page.lines.is_empty());
        let encoded_body: usize = page
            .lines
            .iter()
            .map(|line| serde_json::to_vec(&line.text).unwrap().len())
            .sum();
        assert!(
            serde_json::to_vec(&page).unwrap().len() - encoded_body
                <= read::MAX_PAGE_METADATA_BYTES
        );
        for line in &page.lines {
            recovered.push_str(&line.text);
        }
        req.cursor = page.cursor;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(recovered, content);
}
