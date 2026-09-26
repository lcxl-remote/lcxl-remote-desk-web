//! Shared bounded application catalog and owner references.
use super::*;

impl ComputerUseBroker {
    pub(super) fn inspect_application_catalog(
        &self,
        session_id: &str,
        params: &UiInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<UiInspectOutput, AgentError> {
        let (applications, truncated) = ui_platform::running_applications()?;
        let applications = applications.into_iter().map(|application| {
            let metadata = ui_platform::application_display_metadata(application.process_id);
            (application, metadata)
        });
        let mut output =
            self.project_application_catalog(session_id, params, ceiling, applications)?;
        output.truncated |= truncated;
        Ok(output)
    }

    fn project_application_catalog(
        &self,
        session_id: &str,
        params: &UiInspectParams,
        ceiling: &ComputerUseSettings,
        applications: impl IntoIterator<
            Item = (
                ObservedApplication,
                Option<(String, desk_agent_protocol::computer_use::ApplicationState)>,
            ),
        >,
    ) -> Result<UiInspectOutput, AgentError> {
        let snapshot_id = self.next_snapshot_id();
        let incarnation = format!("{}:{}", session_id, self.current_incarnation_nonce());
        let mut output = UiInspectOutput {
            snapshot_id: snapshot_id.clone(),
            adapter: ComputerUseAdapterRef {
                kind: ui_platform::adapter().0,
                version: ui_platform::adapter().1.into(),
            },
            nodes: Vec::new(),
            owner_selectable_windows: Vec::new(),
            truncated: false,
        };
        for (application, metadata) in applications {
            if !ceiling.application_allowed(&application.image_path) {
                continue;
            }
            let display_name = std::path::Path::new(&application.image_path)
                .file_name()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            let application_state = metadata.as_ref().map(|(_, state)| *state);
            let localized_name = metadata.map(|(name, _)| name).unwrap_or_default();
            let catalog_name = if localized_name.is_empty() || localized_name == display_name {
                display_name.clone()
            } else {
                format!("{localized_name} ({display_name})")
            };
            let matched_queries = params
                .query
                .as_ref()
                .map(|q| {
                    desk_agent_protocol::matching_search_terms(
                        &q.queries,
                        &[&display_name, &localized_name, &application.image_path],
                    )
                })
                .unwrap_or_default();
            if params.query.as_ref().is_some_and(|q| {
                (!q.queries.is_empty() && matched_queries.is_empty()) || q.element_id.is_some()
            }) {
                continue;
            }
            if output.nodes.len() >= (params.max_nodes as usize).min(128) {
                output.truncated = true;
                break;
            }
            let name = Some(catalog_name);
            let object_ref = self.issue_ref(
                &snapshot_id,
                &incarnation,
                ObjectKind::Application,
                ResolvedObject::Application {
                    window_handle: application.window_handle,
                    process_id: application.process_id,
                    image_path: application.image_path,
                    process_started_at: application.process_started_at,
                },
            )?;
            output.nodes.push(UiNodeProjection {
                location: Default::default(),
                application_state,
                element_id: None,
                matched_queries: Vec::new(),
                collapsed_children: 0,
                native_id: None,
                object_ref,
                parent_index: None,
                role: "application".into(),
                name,
                value: None,
                is_protected: false,
                enabled: true,
                supported_actions: Vec::new(),
            });
            if serde_json::to_vec(&output)
                .map_err(|_| {
                    error(
                        AgentErrorKind::Internal,
                        "failed to encode the application catalog",
                        false,
                    )
                })?
                .len()
                > params.max_bytes as usize
            {
                output.nodes.pop();
                output.truncated = true;
                break;
            }
        }
        if serde_json::to_vec(&output)
            .map_err(|_| {
                error(
                    AgentErrorKind::Internal,
                    "failed to encode the application catalog",
                    false,
                )
            })?
            .len()
            > params.max_bytes as usize
        {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "application catalog byte budget is too small",
                false,
            ));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_path(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn params() -> UiInspectParams {
        UiInspectParams {
            root: None,
            scope: Default::default(),
            allow_unfiltered: true,
            query: None,
            overview: false,
            element_only: false,
            max_depth: 16,
            max_nodes: 10,
            max_bytes: 65536,
        }
    }

    fn entry(
        path: &str,
        pid: u32,
    ) -> (
        ObservedApplication,
        Option<(String, desk_agent_protocol::computer_use::ApplicationState)>,
    ) {
        (
            ObservedApplication {
                window_handle: pid as isize,
                process_id: pid,
                image_path: path.into(),
                process_started_at: Some(42),
            },
            None,
        )
    }

    #[test]
    fn catalog_preserves_owner_scope_and_only_issues_application_references() {
        let broker = ComputerUseBroker::new();
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            allowed_application_paths: vec![app_path("allowed-app").as_str().into()],
            ..Default::default()
        };
        let output = broker
            .project_application_catalog(
                "1",
                &params(),
                &ceiling,
                [
                    entry(app_path("other-app").as_str(), 1),
                    entry(app_path("allowed-app").as_str(), 2),
                ],
            )
            .unwrap();
        assert_eq!(output.nodes.len(), 1);
        assert_eq!(output.nodes[0].role, "application");
        assert!(output.nodes[0].supported_actions.is_empty());
        assert!(output.owner_selectable_windows.is_empty());
        assert!(matches!(
            broker.resolve_ref(&output.nodes[0].object_ref).unwrap(),
            ResolvedObject::Application {
                process_id: 2,
                process_started_at: Some(42),
                ..
            }
        ));
        let replacement = ComputerUseBroker::new();
        assert!(
            replacement
                .resolve_ref(&output.nodes[0].object_ref)
                .is_err()
        );
    }

    #[test]
    fn catalog_honors_node_and_byte_budgets_without_expanding_to_ui_contents() {
        let broker = ComputerUseBroker::new();
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            allowed_application_paths: vec![app_path("app").as_str().into()],
            ..Default::default()
        };
        let mut params = params();
        params.max_nodes = 1;
        let output = broker
            .project_application_catalog(
                "1",
                &params,
                &ceiling,
                [
                    entry(app_path("app").as_str(), 1),
                    entry(app_path("app").as_str(), 2),
                ],
            )
            .unwrap();
        assert_eq!(output.nodes.len(), 1);
        assert!(output.truncated);
        params.max_bytes = 1;
        assert_eq!(
            broker
                .project_application_catalog(
                    "1",
                    &params,
                    &ceiling,
                    [entry(app_path("app").as_str(), 1)]
                )
                .unwrap_err()
                .kind,
            AgentErrorKind::OutputLimitExceeded
        );
    }
}
