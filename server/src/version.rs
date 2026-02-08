include!(concat!(env!("OUT_DIR"), "/built_build_number.rs"));

/// The current git commit hash.
pub const SERVER_COMMIT_HASH: &str = env!("SERVER_COMMIT_HASH");
