/// The current Server API version supported by this library.
///
/// A desk server reporting this version understands the fleet exec wire
/// (`EdgeExecRequest` / `EdgeExecResult`), so the manager can dispatch a
/// sealed `ExecPlan` to it. See [`EDGE_EXEC_MIN_SERVER_API_VERSION`].
pub const SERVER_API_VERSION: i32 = 1;

/// The minimum [`SERVER_API_VERSION`] a desk server must report for the manager
/// to dispatch a fleet batch execution to it. The manager checks the target
/// connection's reported `api_version` against this floor **before** claiming a
/// target, so a server that does not understand the fleet exec wire is
/// terminated as `unsupported` rather than left to time out.
pub const EDGE_EXEC_MIN_SERVER_API_VERSION: i32 = 1;
