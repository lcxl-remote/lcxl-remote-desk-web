use super::*;
use actix_web::{App, test};

#[actix_web::test]
async fn policy_drafts_are_checked_without_database_or_config_state() {
    let app = test::init_service(App::new().service(validate_assistant_data_policy)).await;
    for (response_bytes, hits, success, issues) in [
        (32768, 10, true, 0),
        (32768, 100, true, 1),
        (4, 10, false, 1),
    ] {
        let mut policy =
            desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy::default();
        policy.limits.retrieval.episode.response_bytes = response_bytes;
        policy.limits.retrieval.relevance_hits = hits;
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/admin/system/ai-assistant-data/validate")
                .set_json(PolicyCheck { policy })
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), actix_web::http::StatusCode::OK);
        assert_eq!(response.headers().get("Cache-Control").unwrap(), "no-store");
        let body: serde_json::Value = test::read_body_json(response).await;
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
