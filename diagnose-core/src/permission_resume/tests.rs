use super::*;

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway-1".into(),
        connection_revision: 1,
        model_id: "model-1".into(),
        profile_revision: 1,
    }
}

#[test]
fn bridge_is_bound_user_content_but_not_a_new_user_requirement() {
    let message = model_bound_permission_resume_message(
        "bridge-1".into(),
        destination(),
        "Write the exact approved value",
    )
    .unwrap();
    assert_eq!(message.role, ChatRole::User);
    assert!(message.text.contains("not authored by the user"));
    assert!(message.text.contains("Write the exact approved value"));
    let envelope = message.data_envelope.as_ref().unwrap();
    assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
    assert_eq!(envelope.allowed_destinations, vec![destination()]);
    assert!(envelope.retention.delete_with_run);
    assert!(is_permission_resume_message(&message));
    assert!(latest_user_requirement(&[message]).is_none());
}

#[test]
fn client_chosen_id_prefix_cannot_hide_a_real_user_message() {
    let user = ChatMessage::text(
        "permission-resume-user-id",
        ChatRole::User,
        "Actual new question",
    );
    assert!(!is_permission_resume_message(&user));
    let bridge =
        model_bound_permission_resume_message("bridge-1".into(), destination(), "Old question")
            .unwrap();
    let messages = vec![user.clone(), bridge.clone()];
    assert_eq!(latest_user_requirement(&messages), Some(&user));
    let mut mismatched = bridge;
    mismatched.message_id = "copied-id".into();
    assert!(!is_permission_resume_message(&mismatched));
}

#[test]
fn exact_projection_is_sensitive_and_expires_without_changing_its_text() {
    let original = ChatMessage::text(
        "system",
        ChatRole::System,
        "Exact recipient and bounded input",
    );
    let bound =
        bind_exact_authorization_system_message(original.clone(), destination(), 70_000).unwrap();
    assert_eq!(bound.text, original.text);
    let envelope = bound.data_envelope.unwrap();
    assert_eq!(envelope.sensitivity, Sensitivity::Sensitive);
    assert_eq!(envelope.retention.expires_at_unix_ms, Some(70_000));
    assert_eq!(envelope.allowed_destinations, vec![destination()]);
}

#[test]
fn invalid_model_destination_never_produces_a_bridge() {
    let invalid = DestinationIdentity::Model {
        connection_id: String::new(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    assert!(
        model_bound_permission_resume_message("bridge".into(), invalid.clone(), "request").is_err()
    );
    assert!(
        bind_exact_authorization_system_message(
            ChatMessage::text("s", ChatRole::System, "scope"),
            invalid,
            70_000
        )
        .is_err()
    );
}
