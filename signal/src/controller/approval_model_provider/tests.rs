use super::*;
use actix_web::ResponseError;

#[tokio::test]
async fn truncated_probe_has_an_actionable_error_and_cannot_validate() {
    let probe = approval_probe_cases().remove(0);
    let turn = ModelTurn {
        stop_reason: StopReason::MaxTokens,
        text: "synthetic-private-output".into(),
        ..Default::default()
    };
    let error = validate_approval_probe_observed(&probe, &turn, None, 0).unwrap_err();
    let response = probe_failure(error, &turn, 512).error_response();
    assert_eq!(response.status(), actix_web::http::StatusCode::OK);
    let body = actix_web::body::to_bytes(response.into_body())
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["success"], false);
    assert_eq!(
        result["code"],
        DeskErrorCode::AI_APPROVAL_PROBE_OUTPUT_TRUNCATED.code()
    );
    assert!(result["message"].as_str().unwrap().contains("512 tokens"));
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("runtime_max_output_tokens")
    );
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("save and retry")
    );
    assert!(!result.to_string().contains("synthetic-private-output"));
}

#[test]
fn other_protocol_failures_remain_distinct_from_output_truncation() {
    let turn = ModelTurn {
        stop_reason: StopReason::Other,
        ..Default::default()
    };
    for error in [ApprovalProbeError::Protocol, ApprovalProbeError::Verdict] {
        let failure = probe_failure(error, &turn, 512);
        let DeskSignalError::CustomError(detail) = failure else {
            panic!("expected custom error")
        };
        assert_eq!(detail.error_code, DeskErrorCode::SYSTEM_ERROR);
    }
    assert!(
        probe_failure(ApprovalProbeError::Protocol, &turn, 512)
            .to_string()
            .contains("stop_reason=Other")
    );
}
