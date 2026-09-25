//! Internal output catalog. Input ToolSpec remains model-protocol compatible;
//! this table documents the business payload and delivery possibilities.

use crate::chat::{ChatMessage, ChatRole};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryShape {
    Inline,
    InlineOrAttachment,
    InlineBackgroundOrAttachment,
}

impl DeliveryShape {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::InlineOrAttachment => "inline | externalized_attachment",
            Self::InlineBackgroundOrAttachment => {
                "inline | background_completion | externalized_attachment"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputContract {
    pub tool: &'static str,
    pub business_type: &'static str,
    /// Version of this catalog entry, not an assertion that every payload has
    /// a schema_version field.
    pub contract_version: u16,
    pub delivery: DeliveryShape,
}

macro_rules! catalog {
    ($(($tool:literal, $ty:literal, $shape:ident)),+ $(,)?) => {
        pub const ALL: &[OutputContract] = &[
            $(OutputContract { tool: $tool, business_type: $ty, contract_version: 1,
                delivery: DeliveryShape::$shape }),+
        ];
    };
}

catalog!(
    ("update_task_status", "TaskStatusUpdateReceipt", Inline),
    ("control_goal", "GoalControlReceipt", Inline),
    ("request_goal", "GoalRequestReceipt", Inline),
    ("request_directory", "DirectoryRequestReceipt", Inline),
    (
        "request_scheduled_task",
        "ScheduledTaskRequestReceipt",
        Inline
    ),
    ("list_scheduled_tasks", "ScheduledTaskList", Inline),
    (
        "cancel_scheduled_task",
        "ScheduledTaskCancellationReceipt",
        Inline
    ),
    ("request_permissions", "PermissionRequestReceipt", Inline),
    ("describe_tools", "ToolDescriptionList", Inline),
    ("read_conversation_attachment", "ReadPage", Inline),
    ("load_conversation_history", "HistoryPage", Inline),
    (
        "wait_for_task",
        "TaskWaitReceipt + original typed result",
        InlineOrAttachment
    ),
    (
        "read_system_info",
        "ReadContext.SystemInfo",
        InlineOrAttachment
    ),
    (
        "read_process_list",
        "ReadContext.ProcessList",
        InlineOrAttachment
    ),
    (
        "read_network_ports",
        "ReadContext.NetworkPorts",
        InlineOrAttachment
    ),
    (
        "read_service_status",
        "ReadContext.ServiceStatus",
        InlineOrAttachment
    ),
    (
        "read_recent_logs",
        "ReadContext.LogRecent",
        InlineOrAttachment
    ),
    (
        "read_container_list",
        "ReadContext.ContainerList",
        InlineOrAttachment
    ),
    (
        "exec_command",
        "ExecOutput / CommandReceipt",
        InlineBackgroundOrAttachment
    ),
    (
        "read_terminal_output",
        "ReadContext.TerminalOutputInspect",
        InlineOrAttachment
    ),
    (
        "list_applications",
        "ApplicationCatalogPage",
        InlineOrAttachment
    ),
    (
        "launch_application",
        "ComputerActionCompleted<LaunchApplicationResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "inspect_desktop_session",
        "ReadContext.DesktopSessionInspect",
        InlineOrAttachment
    ),
    (
        "inspect_desktop_ui",
        "ReadContext.DesktopUiInspect",
        InlineOrAttachment
    ),
    (
        "read_current_screen",
        "ReadContext.ScreenCaptureCurrent",
        InlineOrAttachment
    ),
    (
        "execute_ui_actions",
        "ApplicationBatchDispatchReceipt",
        InlineBackgroundOrAttachment
    ),
    (
        "send_background_input",
        "ApplicationBatchDispatchReceipt",
        InlineBackgroundOrAttachment
    ),
    (
        "send_raw_input",
        "ComputerActionCompleted",
        InlineBackgroundOrAttachment
    ),
    (
        "preview_computer_action",
        "ComputerActionDraft",
        InlineOrAttachment
    ),
    (
        "browser_open_page",
        "BrowserActionResult / ComputerActionCompleted<BrowserActionResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "browser_navigate_page",
        "BrowserActionResult / ComputerActionCompleted<BrowserActionResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "browser_take_snapshot",
        "BrowserActionResult / ComputerActionCompleted<BrowserActionResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "browser_wait_for",
        "BrowserActionResult / ComputerActionCompleted<BrowserActionResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "browser_fill_form",
        "BrowserActionResult / ComputerActionCompleted<BrowserActionResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "browser_activate_element",
        "BrowserActionResult / ComputerActionCompleted<BrowserActionResult>",
        InlineBackgroundOrAttachment
    ),
    (
        "inspect_files",
        "ReadContext.FileMetadataInspect",
        InlineOrAttachment
    ),
    (
        "read_text_file",
        "ReadContext.FileContentRead",
        InlineOrAttachment
    ),
    (
        "create_text_file",
        "ComputerActionCompleted<CreatedFileArtifactOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "update_text_file",
        "ComputerActionCompleted<TextFileMutationOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "delete_text_file",
        "ComputerActionCompleted<TextFileMutationOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "inspect_spreadsheets",
        "ReadContext.SpreadsheetFileInspect",
        InlineOrAttachment
    ),
    (
        "preview_spreadsheet_merge",
        "ReadContext.SpreadsheetMergePreview",
        InlineOrAttachment
    ),
    (
        "create_workbook",
        "ComputerActionCompleted<CreatedFileArtifactOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "create_formula_workbook",
        "ComputerActionCompleted<CreatedFileArtifactOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "create_word_report",
        "ComputerActionCompleted<CreatedFileArtifactOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "preview_document",
        "ReadContext.DocumentPreview",
        InlineOrAttachment
    ),
    (
        "convert_document",
        "ComputerActionCompleted<DocumentArtifactOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "inspect_office_selection",
        "ReadContext.OfficeInspect",
        InlineOrAttachment
    ),
    (
        "inspect_live_spreadsheet",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_live_document",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_live_presentation",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "patch_live_spreadsheet_cell",
        "ComputerActionCompleted",
        InlineBackgroundOrAttachment
    ),
    (
        "replace_live_document_body",
        "ComputerActionCompleted",
        InlineBackgroundOrAttachment
    ),
    (
        "patch_live_presentation_slide",
        "ComputerActionCompleted",
        InlineBackgroundOrAttachment
    ),
    (
        "inspect_numbers_file",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_pages_file",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_keynote_file",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "patch_numbers_copy",
        "ComputerActionCompleted<BatchDocumentArtifact>",
        InlineBackgroundOrAttachment
    ),
    (
        "replace_pages_copy_body",
        "ComputerActionCompleted<BatchDocumentArtifact>",
        InlineBackgroundOrAttachment
    ),
    (
        "patch_keynote_copy",
        "ComputerActionCompleted<BatchDocumentArtifact>",
        InlineBackgroundOrAttachment
    ),
    (
        "inspect_excel_cell",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_word_file",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_powerpoint_file",
        "ReadContext.LiveDocumentInspect",
        InlineOrAttachment
    ),
    (
        "patch_excel_copy",
        "ComputerActionCompleted<BatchDocumentArtifact>",
        InlineBackgroundOrAttachment
    ),
    (
        "replace_word_copy_body",
        "ComputerActionCompleted<BatchDocumentArtifact>",
        InlineBackgroundOrAttachment
    ),
    (
        "patch_powerpoint_copy",
        "ComputerActionCompleted<BatchDocumentArtifact>",
        InlineBackgroundOrAttachment
    ),
    ("fetch_public_web_page", "FetchOutput", InlineOrAttachment),
    ("search_public_web", "SearchOutput", InlineOrAttachment),
    (
        "create_local_message_draft",
        "ComputerActionCompleted<CreatedFileArtifactOutput>",
        InlineBackgroundOrAttachment
    ),
    (
        "prepare_outlook_draft",
        "CommunicationDraftHandoff",
        InlineBackgroundOrAttachment
    ),
    (
        "prepare_gmail_draft",
        "CommunicationDraftHandoff",
        InlineBackgroundOrAttachment
    ),
    (
        "prepare_slack_message",
        "CommunicationDraftHandoff",
        InlineBackgroundOrAttachment
    ),
    (
        "send_gmail_message",
        "SendReceipt",
        InlineBackgroundOrAttachment
    ),
    (
        "send_slack_message",
        "SendReceipt",
        InlineBackgroundOrAttachment
    ),
);

pub fn for_tool(name: &str) -> Option<&'static OutputContract> {
    ALL.iter().find(|entry| entry.tool == name)
}

/// This is a model-only hint; durable typed results remain unchanged. A
/// returned tool message or valid receipt never proves the user's goal.
pub(crate) fn project_status(message: &mut ChatMessage) {
    if message.role != ChatRole::Tool {
        return;
    }
    let Some(contract) = message
        .data_envelope
        .as_ref()
        .and_then(|envelope| for_tool(&envelope.provenance.source_tool_name))
    else {
        return;
    };
    // These entire receipts are copied verbatim into a later exact-send input.
    // Adding an unknown member would invalidate that typed handoff.
    if matches!(
        contract.tool,
        "prepare_outlook_draft" | "prepare_gmail_draft" | "prepare_slack_message"
    ) {
        return;
    }
    let Ok(mut payload) = serde_json::from_str::<Value>(&message.text) else {
        return;
    };
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    if object.contains_key("result_summary") {
        return;
    }
    let result_status = object
        .get("result")
        .and_then(Value::as_str)
        .or_else(|| object.get("status").and_then(Value::as_str))
        .map(str::to_owned);
    let business_effect = object
        .get("business_effect")
        .and_then(Value::as_str)
        .map(str::to_owned);
    object.insert("result_summary".into(), json!({
        "output_type": contract.business_type,
        "contract_version": contract.contract_version,
        "tool_call_status": match message.tool_ok { Some(true) => "returned_ok", Some(false) => "returned_error", None => "returned_unclassified" },
        "result_status": result_status,
        "business_effect": business_effect,
    }));
    message.text = payload.to_string();
}

pub fn markdown() -> String {
    let mut document = String::from(
        "# AI Assistant output contracts\n\nGenerated from `diagnose-core/src/output_contracts.rs`. Catalog version describes this mapping, not a payload schema field. All errors may instead be returned as safe text; an externalized attachment must be read before interpreting its business payload.\n\n| Tool | Business output type | Catalog version | Possible delivery |\n|---|---|---:|---|\n",
    );
    for contract in ALL {
        document.push_str(&format!(
            "| `{}` | `{}` | {} | {} |\n",
            contract.tool,
            contract.business_type,
            contract.contract_version,
            contract.delivery.label()
        ));
    }
    document
}

pub fn markdown_zh() -> String {
    let mut document = String::from(
        "# AI 助手出参契约目录\n\n由 `diagnose-core/src/output_contracts.rs` 生成。目录版本是映射版本，并不表示每个业务对象都有 `schema_version` 字段。错误可能以安全文本返回；结果外置时须先读取附件，再判断业务内容。\n\n| 工具 | 业务出参类型 | 目录版本 | 可能的投递形态 |\n|---|---|---:|---|\n",
    );
    for contract in ALL {
        document.push_str(&format!(
            "| `{}` | `{}` | {} | {} |\n",
            contract.tool,
            contract.business_type,
            contract.contract_version,
            contract.delivery.label()
        ));
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_registered_tool_has_one_output_contract() {
        let mut seen = HashSet::new();
        for contract in ALL {
            assert!(seen.insert(contract.tool), "duplicate {}", contract.tool);
            assert!(!contract.business_type.is_empty());
            assert!(contract.contract_version > 0);
        }
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        for tool in registry.registered_tools() {
            assert!(
                for_tool(&tool.spec.name).is_some(),
                "missing {}",
                tool.spec.name
            );
        }
        for builtin in [
            "update_task_status",
            "control_goal",
            "request_goal",
            "request_directory",
            "request_scheduled_task",
            "list_scheduled_tasks",
            "cancel_scheduled_task",
            "request_permissions",
            "describe_tools",
            "read_conversation_attachment",
            "load_conversation_history",
            "wait_for_task",
        ] {
            assert!(for_tool(builtin).is_some());
        }
        assert_eq!(ALL.len(), 74);
    }

    #[test]
    fn generated_reference_is_current() {
        if std::env::var_os("UPDATE_AI_ASSISTANT_OUTPUT_CONTRACTS").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../docs/ai-assistant-output-contracts.md"
                ),
                markdown(),
            )
            .unwrap();
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../docs/zh/ai-assistant-output-contracts.md"
                ),
                markdown_zh(),
            )
            .unwrap();
        }
        let expected = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/ai-assistant-output-contracts.md"
        ))
        .unwrap()
        .replace("\r\n", "\n");
        assert_eq!(expected, markdown());
        let expected_zh = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/zh/ai-assistant-output-contracts.md"
        ))
        .unwrap()
        .replace("\r\n", "\n");
        assert_eq!(expected_zh, markdown_zh());
    }
}
