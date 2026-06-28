include!(concat!(env!("OUT_DIR"), "/built_build_number.rs"));

/// The current git commit hash.
pub const SERVER_COMMIT_HASH: &str = env!("SERVER_COMMIT_HASH");

/// The source repository URL this binary was built from. Empty when the build
/// could not resolve a remote and no `SERVER_REPOSITORY_URL` override was set.
pub const SERVER_REPOSITORY_URL: &str = env!("SERVER_REPOSITORY_URL");
