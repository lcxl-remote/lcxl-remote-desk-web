//! The read+exec tool registry helper.
//!
//! [`agent_tool_registry`] is the read-only tool set plus the mutating
//! `exec_command` tool, each mapping onto one [`desk_diagnose_core`] tool.

use desk_diagnose_core::registry::RegisteredTool;

/// The read-only tools plus the mutating exec tool.
pub fn agent_tool_registry() -> Vec<RegisteredTool> {
    let mut reg = desk_diagnose_core::read_tools::read_tool_registry();
    reg.extend(desk_diagnose_core::exec_tools::exec_tool_registry());
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The combined registry exposes the read tools plus the exec tool.
    #[test]
    fn agent_registry_includes_exec_tool() {
        let names: Vec<_> = agent_tool_registry()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"exec_command".to_string()));
        assert!(names.contains(&"read_system_info".to_string()));
    }
}
