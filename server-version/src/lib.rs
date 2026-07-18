/// The current Server API version supported by this library.
///
/// A desk server reporting **v2** understands the source-tagged `EdgeExecRequest`
/// envelope (`EdgeExecRequestPayload::{Fleet | Agentic}`), so the manager can
/// dispatch a sealed `ExecPlan` to it. v1 daemons only understood the earlier bare
/// `ExecPlan` frame and reject the tagged envelope as malformed, so the manager
/// gates edge exec on [`EDGE_EXEC_MIN_SERVER_API_VERSION`]. Edge daemons upgrade
/// independently of the manager, so this skew is permanent, not a rollout window.
pub const SERVER_API_VERSION: i32 = 2;

/// The minimum [`SERVER_API_VERSION`] a desk server must report for the manager to
/// dispatch an edge execution (fleet **or** agentic) to it. The manager checks the
/// target connection's reported `api_version` against this floor **before** sending,
/// so a server that does not understand the source-tagged exec wire is terminated as
/// `unsupported` rather than sent a payload it would reject as malformed. Raised to
/// 2 when the bare-`ExecPlan` frame became the tagged `EdgeExecRequestPayload`.
pub const EDGE_EXEC_MIN_SERVER_API_VERSION: i32 = 2;
