use super::*;
use actix_web::{body::to_bytes, http::StatusCode};

#[actix_web::test]
async fn policy_assessment_reports_issues_without_caching() {
    for (response_bytes, hits, success, issues) in [
        (32768, 10, true, 0),
        (32768, 100, true, 1),
        (4, 10, false, 1),
    ] {
        let mut policy =
            desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy::default();
        policy.limits.retrieval.episode.response_bytes = response_bytes;
        policy.limits.retrieval.relevance_hits = hits;
        let response = policy_assessment(policy_rules::assess(&policy));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("Cache-Control").unwrap(), "no-store");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(body["success"], success);
        assert_eq!(body["data"]["issues"].as_array().unwrap().len(), issues);
        assert_eq!(
            body["code"],
            if success {
                DeskErrorCode::SUCCESS.code()
            } else {
                DeskErrorCode::INVALID_PARAMS.code()
            }
        );
    }
}
