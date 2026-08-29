use desk_agent_protocol::browser_control::{
    BrowserAction, BrowserActionOutcome, BrowserControlContractError, BrowserWaitState,
};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::AllowedChromeMcpTool;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChromeMcpActionPlan {
    pub tool: AllowedChromeMcpTool,
    pub arguments: Map<String, Value>,
    pub outcome: BrowserActionOutcome,
    pub includes_snapshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChromeMcpActionError {
    InvalidContract(BrowserControlContractError),
    InvalidPageId,
    UnsupportedWaitState,
    MissingMaterializedUpload,
}

pub(super) fn plan_action(
    action: &BrowserAction,
) -> Result<ChromeMcpActionPlan, ChromeMcpActionError> {
    action
        .validate()
        .map_err(ChromeMcpActionError::InvalidContract)?;

    let (tool, arguments, outcome, includes_snapshot) = match action {
        BrowserAction::OpenPage { target } => (
            AllowedChromeMcpTool::NewPage,
            map([
                ("url", json!(target.url)),
                ("background", json!(false)),
                ("timeout", json!(30_000)),
            ]),
            BrowserActionOutcome::PageOpened,
            true,
        ),
        BrowserAction::NavigatePage { page, target } => (
            AllowedChromeMcpTool::NavigatePage,
            map([
                ("pageId", json!(page_id(page.page_id.as_str())?)),
                ("type", json!("url")),
                ("url", json!(target.url)),
                ("timeout", json!(30_000)),
                // Never discard unsaved page state by automatically accepting
                // a beforeunload prompt.
                ("handleBeforeUnload", json!("dismiss")),
            ]),
            BrowserActionOutcome::PageNavigated,
            false,
        ),
        BrowserAction::TakeSnapshot { page, .. } => (
            AllowedChromeMcpTool::TakeSnapshot,
            map([
                ("pageId", json!(page_id(page.page_id.as_str())?)),
                ("verbose", json!(false)),
            ]),
            BrowserActionOutcome::SnapshotCaptured,
            true,
        ),
        BrowserAction::WaitFor {
            page,
            element,
            state,
            timeout_ms,
        } => {
            if *state != BrowserWaitState::Present {
                return Err(ChromeMcpActionError::UnsupportedWaitState);
            }
            (
                AllowedChromeMcpTool::WaitFor,
                map([
                    ("pageId", json!(page_id(page.page_id.as_str())?)),
                    ("text", json!([element.accessible_name])),
                    ("timeout", json!(timeout_ms)),
                ]),
                BrowserActionOutcome::WaitSatisfied,
                true,
            )
        }
        BrowserAction::FillForm { page, fields, .. }
        | BrowserAction::FillFormAndUpload { page, fields, .. } => (
            AllowedChromeMcpTool::FillForm,
            map([
                ("pageId", json!(page_id(page.page_id.as_str())?)),
                (
                    "elements",
                    Value::Array(
                        fields
                            .iter()
                            .map(|field| {
                                json!({
                                    "uid": field.element.element_id,
                                    "value": field.value,
                                })
                            })
                            .collect(),
                    ),
                ),
                ("includeSnapshot", json!(true)),
            ]),
            if matches!(action, BrowserAction::FillFormAndUpload { .. }) {
                BrowserActionOutcome::FormFilledWithFile
            } else {
                BrowserActionOutcome::FormFilled
            },
            true,
        ),
        // Upstream requires a native file path, while the shared contract
        // carries only an immutable ContentRef. Until a verified edge artifact
        // materializer is wired, upload is unavailable rather than accepting a
        // model-supplied path.
        BrowserAction::UploadFile { .. } => {
            return Err(ChromeMcpActionError::MissingMaterializedUpload);
        }
        BrowserAction::ActivateElement { page, element, .. } => (
            AllowedChromeMcpTool::Click,
            map([
                ("pageId", json!(page_id(page.page_id.as_str())?)),
                ("uid", json!(element.element_id)),
                // Gmail and Slack can make the upstream click response's
                // embedded snapshot exceed the bounded call deadline after
                // the visible mutation has already happened. Keep the click
                // response small, then capture the required verification
                // snapshot as a separate call while the writer lease is still
                // held by the surrounding semantic action.
                ("includeSnapshot", json!(false)),
            ]),
            BrowserActionOutcome::ElementActivated,
            true,
        ),
    };

    Ok(ChromeMcpActionPlan {
        tool,
        arguments,
        outcome,
        includes_snapshot,
    })
}

pub(super) fn plan_materialized_upload(
    action: &BrowserAction,
    file_path: &Path,
) -> Result<ChromeMcpActionPlan, ChromeMcpActionError> {
    action
        .validate()
        .map_err(ChromeMcpActionError::InvalidContract)?;
    let (page, element, outcome) = match action {
        BrowserAction::UploadFile { page, element, .. } => {
            (page, element, BrowserActionOutcome::FileUploaded)
        }
        BrowserAction::FillFormAndUpload {
            page,
            upload_element,
            ..
        } => (
            page,
            upload_element,
            BrowserActionOutcome::FormFilledWithFile,
        ),
        _ => return Err(ChromeMcpActionError::MissingMaterializedUpload),
    };
    if !file_path.is_absolute() || !file_path.is_file() {
        return Err(ChromeMcpActionError::MissingMaterializedUpload);
    }
    Ok(ChromeMcpActionPlan {
        tool: AllowedChromeMcpTool::UploadFile,
        arguments: map([
            ("pageId", json!(page_id(page.page_id.as_str())?)),
            ("uid", json!(element.element_id)),
            ("filePath", json!(file_path.to_string_lossy())),
            ("includeSnapshot", json!(true)),
        ]),
        outcome,
        includes_snapshot: true,
    })
}

fn page_id(value: &str) -> Result<u64, ChromeMcpActionError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| ChromeMcpActionError::InvalidPageId)?;
    if parsed == 0 || parsed.to_string() != value {
        return Err(ChromeMcpActionError::InvalidPageId);
    }
    Ok(parsed)
}

fn map<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserActivationClass, BrowserAdapterRef,
        BrowserElementRef, BrowserElementRole, BrowserEngineKind, BrowserFormField,
        BrowserMutationClass, BrowserNavigationTarget, BrowserOrigin, BrowserOriginKind,
        BrowserPageRef,
    };
    use desk_agent_protocol::{
        computer_use::{ObjectKind, ObjectRef},
        data_lineage::ContentRef,
    };

    use super::*;

    fn page(page_id: &str) -> BrowserPageRef {
        BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeDevtoolsMcp,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 151,
                browser_version: "151.0.7922.174".into(),
                adapter_id: "chrome-devtools-mcp".into(),
                adapter_version: "1.7.0".into(),
                profile_incarnation: "profile-1".into(),
                connection_revision: 1,
            },
            page_id: page_id.into(),
            page_incarnation: "page-1".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::HttpLoopback,
                host_ascii: "127.0.0.1".into(),
                port: 5174,
            },
            document_revision: 1,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 1,
        }
    }

    fn element(page: &BrowserPageRef) -> BrowserElementRef {
        BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "uid-7".into(),
            role: BrowserElementRole::Textbox,
            accessible_name: "Subject".into(),
            value: None,
            element_revision: 1,
        }
    }

    #[test]
    fn open_and_navigation_have_a_closed_argument_surface() {
        let target = BrowserNavigationTarget {
            url: "http://127.0.0.1:5174/compose".into(),
            origin: page("7").origin,
        };
        let open = plan_action(&BrowserAction::OpenPage {
            target: target.clone(),
        })
        .unwrap();
        assert_eq!(open.tool, AllowedChromeMcpTool::NewPage);
        assert!(open.includes_snapshot);
        assert_eq!(
            open.arguments
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["background", "timeout", "url"]
        );

        let navigate = plan_action(&BrowserAction::NavigatePage {
            page: page("7"),
            target,
        })
        .unwrap();
        assert!(!navigate.includes_snapshot);
        assert_eq!(navigate.arguments["handleBeforeUnload"], "dismiss");
        assert!(!navigate.arguments.contains_key("initScript"));
    }

    #[test]
    fn noncanonical_page_ids_and_nonpresent_waits_fail_closed() {
        let invalid = page("007");
        assert_eq!(
            plan_action(&BrowserAction::TakeSnapshot {
                page: invalid,
                max_elements: 10,
            },),
            Err(ChromeMcpActionError::InvalidPageId)
        );

        let page = page("7");
        assert_eq!(
            plan_action(&BrowserAction::WaitFor {
                element: element(&page),
                page,
                state: BrowserWaitState::Enabled,
                timeout_ms: 1_000,
            },),
            Err(ChromeMcpActionError::UnsupportedWaitState)
        );
    }

    #[test]
    fn mutations_cannot_add_script_or_local_path_arguments() {
        let page = page("7");
        let fill = plan_action(&BrowserAction::FillForm {
            fields: vec![desk_agent_protocol::browser_control::BrowserFormField {
                element: element(&page),
                value: "Draft subject".into(),
            }],
            page: page.clone(),
            mutation_class: BrowserMutationClass::WriteExternalDraft,
        })
        .unwrap();
        assert_eq!(fill.tool, AllowedChromeMcpTool::FillForm);
        assert!(fill.includes_snapshot);
        assert!(!fill.arguments.contains_key("function"));
        assert!(!fill.arguments.contains_key("filePath"));

        let click = plan_action(&BrowserAction::ActivateElement {
            page: page.clone(),
            element: element(&page),
            activation_class: BrowserActivationClass::InputFallback,
        })
        .unwrap();
        assert_eq!(click.tool, AllowedChromeMcpTool::Click);
        assert_eq!(click.arguments["includeSnapshot"], false);
    }

    #[test]
    fn upload_path_is_added_only_after_artifact_materialization() {
        let page = page("7");
        let file = ObjectRef {
            token: "artifact-token-1".into(),
            snapshot_id: "worker-1:1".into(),
            object_kind: ObjectKind::File,
            expires_at: "2026-08-29T06:00:00Z".into(),
        };
        let action = BrowserAction::UploadFile {
            page: page.clone(),
            element: element(&page),
            file: file.clone(),
            content: ContentRef::Artifact {
                artifact_id: file.token.clone(),
                sha256: "c".repeat(64),
                size_bytes: 3,
                media_type: "application/test".into(),
            },
            file_name: "report.docx".into(),
            media_type: "application/test".into(),
            size_bytes: 3,
            digest_sha256: "c".repeat(64),
            mutation_class: BrowserMutationClass::WriteExternalDraft,
        };
        assert_eq!(
            plan_action(&action),
            Err(ChromeMcpActionError::MissingMaterializedUpload)
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.docx");
        std::fs::write(&path, b"doc").unwrap();
        let plan = plan_materialized_upload(&action, &path).unwrap();
        assert_eq!(plan.tool, AllowedChromeMcpTool::UploadFile);
        assert_eq!(plan.arguments["pageId"], 7);
        assert_eq!(plan.arguments["uid"], "uid-7");
        assert_eq!(plan.arguments["filePath"], path.to_string_lossy().as_ref());
        assert_eq!(plan.arguments["includeSnapshot"], true);

        let composite = BrowserAction::FillFormAndUpload {
            page: page.clone(),
            fields: vec![BrowserFormField {
                element: BrowserElementRef {
                    element_id: "subject-1".into(),
                    accessible_name: "Subject".into(),
                    ..element(&page)
                },
                value: "Quarterly report".into(),
            }],
            upload_element: element(&page),
            file: file.clone(),
            content: ContentRef::Artifact {
                artifact_id: file.token,
                sha256: "c".repeat(64),
                size_bytes: 3,
                media_type: "application/test".into(),
            },
            file_name: "report.docx".into(),
            media_type: "application/test".into(),
            size_bytes: 3,
            digest_sha256: "c".repeat(64),
            mutation_class: BrowserMutationClass::WriteExternalDraft,
        };
        let fill_plan = plan_action(&composite).unwrap();
        assert_eq!(fill_plan.tool, AllowedChromeMcpTool::FillForm);
        assert_eq!(fill_plan.outcome, BrowserActionOutcome::FormFilledWithFile);
        let upload_plan = plan_materialized_upload(&composite, &path).unwrap();
        assert_eq!(upload_plan.tool, AllowedChromeMcpTool::UploadFile);
        assert_eq!(
            upload_plan.outcome,
            BrowserActionOutcome::FormFilledWithFile
        );
    }
}
