//! Traverse server-verified provenance, never model-declared permission summaries.
pub mod attachment;

use crate::model_egress::ModelInputLineage;
use desk_agent_protocol::data_lineage::DataProvenance;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSourceAuthority {
    /// Scope independently established from the original successful read or fixed input.
    /// A read can introduce this scope while also depending on upstream sources.
    Scopes(Vec<String>),
    /// Only the server's public system prompt needs no user-data source scope.
    SystemPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Independently verified authority at a node; never a traversal stopping point.
pub struct TaskSourceBinding {
    pub envelope_id: String,
    pub digest_sha256: String,
    pub authority: TaskSourceAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTaskSources {
    pub scopes: Vec<String>,
    pub root_envelope_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSourceError {
    InvalidNode,
    ConflictingNode,
    InvalidRoot,
    MissingSource,
    Cycle,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.trim() == id && !id.chars().any(char::is_control)
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Callers must first verify every model/transform receipt and root authority in
/// the same owner-bound frozen run. This verifies graph completeness and scope
/// union only; it neither establishes root permissions nor authorizes any sink.
/// Traversal is iterative so a deep history cannot exhaust the call stack.
pub fn resolve_task_sources(
    output_id: &str,
    output_digest: &str,
    nodes: &[ModelInputLineage],
    bindings: &[TaskSourceBinding],
) -> Result<ResolvedTaskSources, TaskSourceError> {
    let mut indexed = BTreeMap::new();
    for node in nodes {
        if !valid_id(&node.envelope_id)
            || !valid_digest(&node.digest_sha256)
            || node.source_envelope_ids.iter().any(|id| !valid_id(id))
            || node
                .source_envelope_ids
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != node.source_envelope_ids.len()
        {
            return Err(TaskSourceError::InvalidNode);
        }
        DataProvenance {
            source_provider_id: node.source_provider_id.clone(),
            source_tool_name: node.source_tool_name.clone(),
            source_object_id: None,
            source_envelope_ids: node.source_envelope_ids.clone(),
        }
        .validate()
        .map_err(|_| TaskSourceError::InvalidNode)?;
        if indexed
            .insert(node.envelope_id.as_str(), node)
            .is_some_and(|previous| previous != node)
        {
            return Err(TaskSourceError::ConflictingNode);
        }
    }
    let mut root_map = BTreeMap::new();
    for root in bindings {
        let node = indexed
            .get(root.envelope_id.as_str())
            .ok_or(TaskSourceError::InvalidRoot)?;
        if root.digest_sha256 != node.digest_sha256 {
            return Err(TaskSourceError::InvalidRoot);
        }
        match &root.authority {
            TaskSourceAuthority::Scopes(scopes)
                if !scopes.is_empty()
                    && scopes.iter().all(|scope| valid_id(scope))
                    && scopes.iter().collect::<BTreeSet<_>>().len() == scopes.len() => {}
            TaskSourceAuthority::SystemPrompt
                if crate::model_egress::is_audited_public_system_prompt(node) => {}
            _ => return Err(TaskSourceError::InvalidRoot),
        }
        if root_map
            .insert(root.envelope_id.as_str(), root)
            .is_some_and(|previous| previous != root)
        {
            return Err(TaskSourceError::InvalidRoot);
        }
    }
    let output = indexed
        .get(output_id)
        .ok_or(TaskSourceError::MissingSource)?;
    if output.digest_sha256 != output_digest {
        return Err(TaskSourceError::ConflictingNode);
    }
    let mut pending = vec![(output_id, false)];
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    let mut scopes = BTreeSet::new();
    let mut reached_roots = BTreeSet::new();
    while let Some((id, exiting)) = pending.pop() {
        if exiting {
            active.remove(id);
            done.insert(id);
            continue;
        }
        if done.contains(id) {
            continue;
        }
        if !active.insert(id) {
            return Err(TaskSourceError::Cycle);
        }
        let node = indexed.get(id).ok_or(TaskSourceError::MissingSource)?;
        let authority = root_map.get(id);
        if let Some(TaskSourceBinding {
            authority: TaskSourceAuthority::Scopes(allowed),
            ..
        }) = authority
        {
            scopes.extend(allowed.iter().cloned());
        }
        if node.source_envelope_ids.is_empty() {
            authority.ok_or(TaskSourceError::MissingSource)?;
            reached_roots.insert(id.to_owned());
            active.remove(id);
            done.insert(id);
        } else {
            pending.push((id, true));
            pending.extend(
                node.source_envelope_ids
                    .iter()
                    .rev()
                    .map(|parent| (parent.as_str(), false)),
            );
        }
    }
    Ok(ResolvedTaskSources {
        scopes: scopes.into_iter().collect(),
        root_envelope_ids: reached_roots.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(id: &str, parents: &[&str]) -> ModelInputLineage {
        ModelInputLineage {
            public_system_prompt: false,
            envelope_id: id.into(),
            digest_sha256: "a".repeat(64),
            source_provider_id: "device".into(),
            source_tool_name: "read".into(),
            source_envelope_ids: parents.iter().map(|id| (*id).into()).collect(),
        }
    }
    fn root(id: &str) -> TaskSourceBinding {
        TaskSourceBinding {
            envelope_id: id.into(),
            digest_sha256: "a".repeat(64),
            authority: TaskSourceAuthority::Scopes(vec![format!("scope:{id}")]),
        }
    }
    #[test]
    fn follows_exports_and_shared_ancestors_without_adding_unused_scopes() {
        let nodes = vec![
            node("read", &[]),
            node("unused", &[]),
            node("export", &["read"]),
            node("previous", &["export"]),
            node("output", &["export", "previous"]),
        ];
        let proof = resolve_task_sources(
            "output",
            &"a".repeat(64),
            &nodes,
            &[root("read"), root("unused")],
        )
        .unwrap();
        assert_eq!(proof.scopes, ["scope:read"]);
        assert_eq!(proof.root_envelope_ids, ["read"]);
    }
    #[test]
    fn read_authority_adds_scope_without_hiding_its_input_dependencies() {
        let nodes = vec![
            node("question", &[]),
            node("read", &["question"]),
            node("output", &["read"]),
        ];
        let proof = resolve_task_sources(
            "output",
            &"a".repeat(64),
            &nodes,
            &[root("read"), root("question")],
        )
        .unwrap();
        assert_eq!(proof.scopes, ["scope:question", "scope:read"]);
        assert_eq!(proof.root_envelope_ids, ["question"]);
        assert_eq!(
            resolve_task_sources("output", &"a".repeat(64), &nodes, &[root("read")]),
            Err(TaskSourceError::MissingSource),
        );
        let cyclic = vec![node("read", &["output"]), node("output", &["read"])];
        assert_eq!(
            resolve_task_sources("output", &"a".repeat(64), &cyclic, &[root("read")]),
            Err(TaskSourceError::Cycle),
        );
    }

    #[test]
    fn rejects_missing_cyclic_conflicting_and_unproven_sources() {
        let digest = "a".repeat(64);
        assert!(
            resolve_task_sources("output", &digest, &[node("output", &["missing"])], &[]).is_err()
        );
        assert!(resolve_task_sources("output", &digest, &[node("output", &[])], &[]).is_err());
        assert_eq!(
            resolve_task_sources(
                "output",
                &digest,
                &[node("output", &["child"]), node("child", &["output"])],
                &[]
            ),
            Err(TaskSourceError::Cycle)
        );
        let mut conflict = node("root", &[]);
        conflict.digest_sha256 = "b".repeat(64);
        assert_eq!(
            resolve_task_sources(
                "root",
                &digest,
                &[node("root", &[]), conflict],
                &[root("root")]
            ),
            Err(TaskSourceError::ConflictingNode)
        );
        assert!(
            resolve_task_sources(
                "output",
                &digest,
                &[node("root", &[]), node("output", &["root"])],
                &[root("output")]
            )
            .is_err()
        );
        let mut wrong = root("root");
        wrong.digest_sha256 = "b".repeat(64);
        assert!(resolve_task_sources("root", &digest, &[node("root", &[])], &[wrong]).is_err());
        let mut public = root("root");
        public.authority = TaskSourceAuthority::SystemPrompt;
        assert!(resolve_task_sources("root", &digest, &[node("root", &[])], &[public]).is_err());
    }
}
