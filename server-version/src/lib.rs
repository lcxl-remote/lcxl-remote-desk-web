/// The current Server API version supported by this library.
///
/// Bumped to `2` when fleet batch execution (`FleetExecRequest` /
/// `FleetExecResult`) landed: a desk server reporting `>= 2` understands the
/// fleet exec wire, so the manager can dispatch a sealed `ExecPlan` to it. A
/// server reporting `1` predates fleet exec and the manager must not write a
/// dispatch intent against it (it would never answer, producing a false
/// needs-review). See [`FLEET_EXEC_MIN_SERVER_API_VERSION`].
pub const SERVER_API_VERSION: i32 = 2;

/// The minimum [`SERVER_API_VERSION`] a desk server must report for the manager
/// to dispatch a fleet batch execution to it. The manager checks the target
/// connection's reported `api_version` against this floor **before** claiming a
/// target, so an older daemon is terminated as `unsupported` rather than left to
/// time out.
pub const FLEET_EXEC_MIN_SERVER_API_VERSION: i32 = 2;
