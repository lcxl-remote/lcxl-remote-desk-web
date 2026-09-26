# AI 助手出参契约目录

由 `diagnose-core/src/output_contracts.rs` 生成。目录版本是映射版本，并不表示每个业务对象都有 `schema_version` 字段。错误可能以安全文本返回；结果外置时须先读取附件，再判断业务内容。

| 工具 | 业务出参类型 | 目录版本 | 可能的投递形态 |
|---|---|---:|---|
| `update_task_status` | `TaskStatusUpdateReceipt` | 1 | inline |
| `control_goal` | `GoalControlReceipt` | 1 | inline |
| `request_goal` | `GoalRequestReceipt` | 1 | inline |
| `request_directory` | `DirectoryRequestReceipt` | 1 | inline |
| `request_scheduled_task` | `ScheduledTaskRequestReceipt` | 1 | inline |
| `list_scheduled_tasks` | `ScheduledTaskList` | 1 | inline |
| `cancel_scheduled_task` | `ScheduledTaskCancellationReceipt` | 1 | inline |
| `request_permissions` | `PermissionRequestReceipt` | 1 | inline |
| `describe_tools` | `ToolDescriptionList` | 1 | inline |
| `read_conversation_attachment` | `ReadPage` | 1 | inline |
| `load_conversation_history` | `HistoryPage` | 1 | inline |
| `wait_for_task` | `TaskWaitReceipt + original typed result` | 1 | inline | externalized_attachment |
| `read_system_info` | `ReadContext.SystemInfo` | 1 | inline | externalized_attachment |
| `read_process_list` | `ReadContext.ProcessList` | 1 | inline | externalized_attachment |
| `read_network_ports` | `ReadContext.NetworkPorts` | 1 | inline | externalized_attachment |
| `read_service_status` | `ReadContext.ServiceStatus` | 1 | inline | externalized_attachment |
| `read_recent_logs` | `ReadContext.LogRecent` | 1 | inline | externalized_attachment |
| `read_container_list` | `ReadContext.ContainerList` | 1 | inline | externalized_attachment |
| `exec_command` | `ExecOutput / CommandReceipt` | 1 | inline | background_completion | externalized_attachment |
| `read_terminal_output` | `ReadContext.TerminalOutputInspect` | 1 | inline | externalized_attachment |
| `list_applications` | `ApplicationCatalogPage` | 1 | inline | externalized_attachment |
| `launch_application` | `ComputerActionCompleted<LaunchApplicationResult>` | 1 | inline | background_completion | externalized_attachment |
| `inspect_desktop_session` | `ReadContext.DesktopSessionInspect` | 1 | inline | externalized_attachment |
| `inspect_desktop_ui` | `ReadContext.DesktopUiInspect` | 1 | inline | externalized_attachment |
| `read_current_screen` | `ReadContext.ScreenCaptureCurrent` | 1 | inline | externalized_attachment |
| `execute_ui_actions` | `ApplicationBatchDispatchReceipt` | 1 | inline | background_completion | externalized_attachment |
| `send_background_input` | `ApplicationBatchDispatchReceipt` | 1 | inline | background_completion | externalized_attachment |
| `send_raw_input` | `ComputerActionCompleted` | 1 | inline | background_completion | externalized_attachment |
| `execute_wayland_output_input` | `ComputerActionCompleted` | 1 | inline | background_completion | externalized_attachment |
| `preview_computer_action` | `ComputerActionDraft` | 1 | inline | externalized_attachment |
| `browser_open_page` | `BrowserActionResult / ComputerActionCompleted<BrowserActionResult>` | 1 | inline | background_completion | externalized_attachment |
| `browser_navigate_page` | `BrowserActionResult / ComputerActionCompleted<BrowserActionResult>` | 1 | inline | background_completion | externalized_attachment |
| `browser_take_snapshot` | `BrowserActionResult / ComputerActionCompleted<BrowserActionResult>` | 1 | inline | background_completion | externalized_attachment |
| `browser_wait_for` | `BrowserActionResult / ComputerActionCompleted<BrowserActionResult>` | 1 | inline | background_completion | externalized_attachment |
| `browser_fill_form` | `BrowserActionResult / ComputerActionCompleted<BrowserActionResult>` | 1 | inline | background_completion | externalized_attachment |
| `browser_activate_element` | `BrowserActionResult / ComputerActionCompleted<BrowserActionResult>` | 1 | inline | background_completion | externalized_attachment |
| `inspect_files` | `ReadContext.FileMetadataInspect` | 1 | inline | externalized_attachment |
| `read_text_file` | `ReadContext.FileContentRead` | 1 | inline | externalized_attachment |
| `create_text_file` | `ComputerActionCompleted<CreatedFileArtifactOutput>` | 1 | inline | background_completion | externalized_attachment |
| `update_text_file` | `ComputerActionCompleted<TextFileMutationOutput>` | 1 | inline | background_completion | externalized_attachment |
| `delete_text_file` | `ComputerActionCompleted<TextFileMutationOutput>` | 1 | inline | background_completion | externalized_attachment |
| `inspect_spreadsheets` | `ReadContext.SpreadsheetFileInspect` | 1 | inline | externalized_attachment |
| `preview_spreadsheet_merge` | `ReadContext.SpreadsheetMergePreview` | 1 | inline | externalized_attachment |
| `create_workbook` | `ComputerActionCompleted<CreatedFileArtifactOutput>` | 1 | inline | background_completion | externalized_attachment |
| `create_formula_workbook` | `ComputerActionCompleted<CreatedFileArtifactOutput>` | 1 | inline | background_completion | externalized_attachment |
| `create_word_report` | `ComputerActionCompleted<CreatedFileArtifactOutput>` | 1 | inline | background_completion | externalized_attachment |
| `preview_document` | `ReadContext.DocumentPreview` | 1 | inline | externalized_attachment |
| `convert_document` | `ComputerActionCompleted<DocumentArtifactOutput>` | 1 | inline | background_completion | externalized_attachment |
| `inspect_office_selection` | `ReadContext.OfficeInspect` | 1 | inline | externalized_attachment |
| `inspect_live_spreadsheet` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `inspect_live_document` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `inspect_live_presentation` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `patch_live_spreadsheet_cell` | `ComputerActionCompleted` | 1 | inline | background_completion | externalized_attachment |
| `replace_live_document_body` | `ComputerActionCompleted` | 1 | inline | background_completion | externalized_attachment |
| `patch_live_presentation_slide` | `ComputerActionCompleted` | 1 | inline | background_completion | externalized_attachment |
| `inspect_numbers_file` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `inspect_pages_file` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `inspect_keynote_file` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `patch_numbers_copy` | `ComputerActionCompleted<BatchDocumentArtifact>` | 1 | inline | background_completion | externalized_attachment |
| `replace_pages_copy_body` | `ComputerActionCompleted<BatchDocumentArtifact>` | 1 | inline | background_completion | externalized_attachment |
| `patch_keynote_copy` | `ComputerActionCompleted<BatchDocumentArtifact>` | 1 | inline | background_completion | externalized_attachment |
| `inspect_excel_cell` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `inspect_word_file` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `inspect_powerpoint_file` | `ReadContext.LiveDocumentInspect` | 1 | inline | externalized_attachment |
| `patch_excel_copy` | `ComputerActionCompleted<BatchDocumentArtifact>` | 1 | inline | background_completion | externalized_attachment |
| `replace_word_copy_body` | `ComputerActionCompleted<BatchDocumentArtifact>` | 1 | inline | background_completion | externalized_attachment |
| `patch_powerpoint_copy` | `ComputerActionCompleted<BatchDocumentArtifact>` | 1 | inline | background_completion | externalized_attachment |
| `fetch_public_web_page` | `FetchOutput` | 1 | inline | externalized_attachment |
| `search_public_web` | `SearchOutput` | 1 | inline | externalized_attachment |
| `create_local_message_draft` | `ComputerActionCompleted<CreatedFileArtifactOutput>` | 1 | inline | background_completion | externalized_attachment |
| `prepare_outlook_draft` | `CommunicationDraftHandoff` | 1 | inline | background_completion | externalized_attachment |
| `prepare_gmail_draft` | `CommunicationDraftHandoff` | 1 | inline | background_completion | externalized_attachment |
| `prepare_slack_message` | `CommunicationDraftHandoff` | 1 | inline | background_completion | externalized_attachment |
| `send_gmail_message` | `SendReceipt` | 1 | inline | background_completion | externalized_attachment |
| `send_slack_message` | `SendReceipt` | 1 | inline | background_completion | externalized_attachment |
