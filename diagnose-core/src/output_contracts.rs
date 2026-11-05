//! Internal output catalog. Input ToolSpec remains model-protocol compatible;
//! this table documents the business payload and delivery possibilities.

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
    ("send_subagent_request", "SubAgentSpawnReceipt", Inline),
    ("list_subagents", "AiAssistantSubAgentPage", Inline),
    ("get_subagent_status", "AiAssistantSubAgentSummary", Inline),
    ("read_subagent_reports", "WorkerReportPage", Inline),
    (
        "acknowledge_subagent_reports",
        "WorkerReportAcknowledgement",
        Inline
    ),
    ("report_subagent_status", "WorkerReportReceipt", Inline),
    (
        "wait_for_subagent_device",
        "WorkerDeviceWaitReceipt",
        Inline
    ),
    (
        "reply_subagent_request",
        "AiAssistantSubAgentSummary",
        Inline
    ),
    (
        "ask_owner_for_subagent",
        "WorkerQuestionHandoffReceipt",
        Inline
    ),
    ("wait_subagent_reports", "SubAgentWaitReceipt", Inline),
    ("cancel_subagent_request", "SubAgentControlReceipt", Inline),
    ("adjust_subagent_request", "SubAgentControlReceipt", Inline),
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
    ("load_skill", "SkillLoadReceipt", Inline),
    ("read_conversation_attachment", "ReadPage", Inline),
    ("load_conversation_history", "HistoryPage", Inline),
    ("search_memories", "MemorySearchResult", Inline),
    ("read_memory_history", "MemoryHistoryResult", Inline),
    ("manage_memories", "MemoryWriteResult", Inline),
    ("read_memory_source", "AssistantSourceResolution", Inline),
    ("search_conversations", "ConversationSearchResult", Inline),
    ("read_conversation", "ConversationReadResult", Inline),
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
        "execute_wayland_output_input",
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
        "ReadContext.OfficeDocumentInspect",
        InlineOrAttachment
    ),
    (
        "inspect_live_spreadsheet",
        "ReadContext.SpreadsheetLiveInspect",
        InlineOrAttachment
    ),
    (
        "inspect_live_document",
        "ReadContext.DocumentLiveInspect",
        InlineOrAttachment
    ),
    (
        "inspect_live_presentation",
        "ReadContext.PresentationLiveInspect",
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
        "ReadContext.SpreadsheetLiveInspect",
        InlineOrAttachment
    ),
    (
        "inspect_pages_file",
        "ReadContext.DocumentLiveInspect",
        InlineOrAttachment
    ),
    (
        "inspect_keynote_file",
        "ReadContext.PresentationLiveInspect",
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
        "ReadContext.SpreadsheetLiveInspect",
        InlineOrAttachment
    ),
    (
        "inspect_word_file",
        "ReadContext.DocumentLiveInspect",
        InlineOrAttachment
    ),
    (
        "inspect_powerpoint_file",
        "ReadContext.PresentationLiveInspect",
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

pub fn markdown() -> String {
    let mut document = String::from(
        "# AI Assistant output contracts\n\nGenerated from `diagnose-core/src/output_contracts.rs`. Catalog version describes this mapping, not a payload schema field. All errors may instead be returned as safe text; an externalized attachment must be read before interpreting its business payload.\n\nThe `read_subagent_reports` tool returns a `WorkerReportPage` with reports in sequence order, acceptance criteria when reports are present, a next cursor, and an indication of further pages. Report text and available evidence and receipt references stay with the individual report.\n\nModel results retain business text, outcomes, times, coverage and usable references. Internal source locators, verification hashes, idempotency keys and control generations stay in the original stored result. History uses conversation aliases such as `cv1`; source IDs remain readable, and memory update/forget keeps the memory revision. Live document references and JSON attachment pages use the same view as inline results. A successful tool return does not by itself prove that an action completed or was verified.\n\n| Tool | Business output type | Catalog version | Possible delivery |\n|---|---|---:|---|\n",
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
pub fn markdown_zh() -> String {
    let mut document = String::from(
        "# AI 助手出参契约目录\n\n由 `diagnose-core/src/output_contracts.rs` 生成。目录版本是映射版本，并不表示每个业务对象都有 `schema_version` 字段。错误可能以安全文本返回；结果外置时须先读取附件，再判断业务内容。\n\n`read_subagent_reports` 返回 `WorkerReportPage`。报告按序号排列，有报告时附验收标准，并给出下一页游标及是否还有后续页。正文和可用的证据、回执引用随每条报告返回。\n\n模型结果保留业务正文、结果、时间、覆盖范围和可用引用。内部来源定位、校验摘要、幂等键和控制代次保存在原始结果中。历史会话使用 `cv1` 等别名；来源 ID 可继续读取，更新和遗忘记忆仍保留记忆版本。实时文档引用、JSON 附件分页与内联结果使用相同视图。工具正常返回本身不代表动作已完成或验证成功。\n\n| 工具 | 业务出参类型 | 目录版本 | 可能的投递形态 |\n|---|---|---:|---|\n",
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
        for tool in crate::subagent::tools::registry() {
            assert!(for_tool(tool.name()).is_some(), "missing {}", tool.name());
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
            "load_skill",
            "read_conversation_attachment",
            "load_conversation_history",
            "search_memories",
            "read_memory_history",
            "manage_memories",
            "read_memory_source",
            "search_conversations",
            "read_conversation",
            "wait_for_task",
        ] {
            assert!(for_tool(builtin).is_some());
        }
        assert_eq!(ALL.len(), 94);
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
