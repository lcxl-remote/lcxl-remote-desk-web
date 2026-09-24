//! Background owner reminders while the native shell is running, even if its
//! webview is hidden. Only state labels and the local attention route leave the
//! server process; goal text and tool arguments are never notification content.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use desk_signal::control_authorizer::SINGLE_ACCOUNT_USER_ID;
use desk_signal_facade::controller::ai_assistant_session::AiAssistantAttentionReason;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const REMINDER_INTERVAL_MS: u64 = 24 * 60 * 60 * 1_000;

fn notification_state_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|directory| directory.join("ai-assistant-attention-reminders.json"))
}

fn label(reason: AiAssistantAttentionReason) -> String {
    match reason {
        AiAssistantAttentionReason::GoalOpenApproval => {
            rust_i18n::t!("ai_attention.goal_open").to_string()
        }
        AiAssistantAttentionReason::PermissionApproval => {
            rust_i18n::t!("ai_attention.permission").to_string()
        }
        AiAssistantAttentionReason::GoalNeedsInput => {
            rust_i18n::t!("ai_attention.needs_input").to_string()
        }
        AiAssistantAttentionReason::GoalBudget => rust_i18n::t!("ai_attention.budget").to_string(),
        AiAssistantAttentionReason::GoalStalled => {
            rust_i18n::t!("ai_attention.stalled").to_string()
        }
        AiAssistantAttentionReason::GoalBlocked => {
            rust_i18n::t!("ai_attention.blocked").to_string()
        }
        AiAssistantAttentionReason::GoalDeadlineSoon => {
            rust_i18n::t!("ai_attention.deadline").to_string()
        }
    }
}

pub fn start(app: AppHandle, attention_url: String) {
    tauri::async_runtime::spawn(async move {
        let Some(path) = notification_state_path(&app) else {
            log::warn!("AI Assistant reminders unavailable: app data directory not found");
            return;
        };
        let mut sent: HashMap<String, u64> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if super::IS_EXITING.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                Err(_) => continue,
            };
            let items = match desk_signal::agent_owner_attention::list_goal_attention(
                desk_signal::db::get_db(),
                &SINGLE_ACCOUNT_USER_ID.to_string(),
                now,
            )
            .await
            {
                Ok(items) => items,
                Err(error) => {
                    log::warn!("AI Assistant reminder scan failed: {error}");
                    continue;
                }
            };
            for item in items {
                if sent
                    .get(&item.attention_id)
                    .is_some_and(|last| now.saturating_sub(*last) < REMINDER_INTERVAL_MS)
                {
                    continue;
                }
                let result = app
                    .notification()
                    .builder()
                    .title(rust_i18n::t!("ai_attention.title"))
                    .body(format!("{}\n{}", label(item.reason), attention_url))
                    .show();
                match result {
                    Ok(()) => {
                        sent.insert(item.attention_id, now);
                        sent.retain(|_, last| {
                            now.saturating_sub(*last) < 30 * REMINDER_INTERVAL_MS
                        });
                        if let Some(directory) = path.parent() {
                            if let Err(error) = std::fs::create_dir_all(directory) {
                                log::warn!(
                                    "AI Assistant reminder state directory unavailable: {error}"
                                );
                                continue;
                            }
                        }
                        if let Ok(bytes) = serde_json::to_vec(&sent)
                            && let Err(error) = std::fs::write(&path, bytes)
                        {
                            log::warn!("AI Assistant reminder state could not be saved: {error}");
                        }
                    }
                    Err(error) => log::warn!("AI Assistant notification failed: {error}"),
                }
            }
        }
    });
}
