include!(concat!(env!("OUT_DIR"), "/built_build_number.rs"));

/// The current git commit hash.
pub const SIGNAL_COMMIT_HASH: &str = env!("SIGNAL_COMMIT_HASH");
