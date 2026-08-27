/// The current Server API version supported by this library.
///
/// Version 1 introduced the source-tagged confirmed-exec envelope. Version 2
/// adds the dedicated Computer Use readiness and mutation protocol family.
pub const SERVER_API_VERSION: i32 = 2;

/// The minimum [`SERVER_API_VERSION`] a desk server must report for the manager to
/// dispatch an edge execution (fleet **or** agentic) to it. The manager checks the
/// target connection's reported `api_version` against this floor **before** sending,
/// so an unversioned server is terminated as `unsupported` rather than sent a
/// confirmed-execution payload.
pub const EDGE_EXEC_MIN_SERVER_API_VERSION: i32 = 1;

/// The first server API that can receive a sealed Computer Use plan. Readiness
/// and action dispatch must both use this floor; OS name alone never implies
/// support.
pub const COMPUTER_USE_MIN_SERVER_API_VERSION: i32 = 2;

/// Whether a reported desk-server API version supports the current confirmed-exec
/// signaling contract. Both manager execution paths use this shared predicate.
pub const fn supports_edge_exec(api_version: i32) -> bool {
    api_version >= EDGE_EXEC_MIN_SERVER_API_VERSION
}

/// Whether the desk server supports the dedicated Computer Use wire family.
pub const fn supports_computer_use(api_version: i32) -> bool {
    api_version >= COMPUTER_USE_MIN_SERVER_API_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_floors_are_independent_and_monotone() {
        assert_eq!(SERVER_API_VERSION, 2);
        assert_eq!(EDGE_EXEC_MIN_SERVER_API_VERSION, 1);
        assert_eq!(COMPUTER_USE_MIN_SERVER_API_VERSION, 2);
        assert!(!supports_edge_exec(0));
        assert!(supports_edge_exec(1));
        assert!(supports_edge_exec(2));
        assert!(!supports_computer_use(0));
        assert!(!supports_computer_use(1));
        assert!(supports_computer_use(2));
        assert!(supports_computer_use(3));
    }
}
