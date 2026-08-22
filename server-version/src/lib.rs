/// The current Server API version supported by this library.
///
/// Version 1 is the first released contract and includes the source-tagged
/// `EdgeExecRequestPayload::{Fleet | Agentic}` envelope. A missing/zero version is
/// treated as lacking confirmed edge execution support.
pub const SERVER_API_VERSION: i32 = 1;

/// The minimum [`SERVER_API_VERSION`] a desk server must report for the manager to
/// dispatch an edge execution (fleet **or** agentic) to it. The manager checks the
/// target connection's reported `api_version` against this floor **before** sending,
/// so an unversioned server is terminated as `unsupported` rather than sent a
/// confirmed-execution payload.
pub const EDGE_EXEC_MIN_SERVER_API_VERSION: i32 = 1;

/// Whether a reported desk-server API version supports the current confirmed-exec
/// signaling contract. Both manager execution paths use this shared predicate.
pub const fn supports_edge_exec(api_version: i32) -> bool {
    api_version >= EDGE_EXEC_MIN_SERVER_API_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_is_the_first_confirmed_exec_contract() {
        assert_eq!(SERVER_API_VERSION, 1);
        assert_eq!(EDGE_EXEC_MIN_SERVER_API_VERSION, 1);
        assert!(!supports_edge_exec(0));
        assert!(supports_edge_exec(1));
        assert!(supports_edge_exec(2));
    }
}
