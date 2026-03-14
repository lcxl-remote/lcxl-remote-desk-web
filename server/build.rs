use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // 1. Get Git Version
    let output = Command::new("git")
        .args(&["rev-list", "--count", "HEAD"])
        .output();
    let build_number = match output {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout).unwrap().trim().to_string(),
        _ => "0".to_string(),
    };

    // 2. Generate build number as a complete Rust code file
    //    so it can be included as a module in version.rs, which is more IDE-friendly
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("built_build_number.rs");

    // Write complete pub const definition with doc comments
    let content = format!(
        "/// The current build number (Git revision count).\n/// This constant is of type i32, suitable for direct integer arithmetic.\npub const SERVER_BUILD_NUMBER: i32 = {};",
        build_number
    );
    fs::write(&dest_path, content).unwrap();

    // 3. Get Git Commit Hash
    let output_hash = Command::new("git").args(&["rev-parse", "HEAD"]).output();
    let commit_hash = match output_hash {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout).unwrap().trim().to_string(),
        _ => "unknown".to_string(),
    };

    // 4. Set commit hash as environment variable (rustc-env)
    println!("cargo:rustc-env=SERVER_COMMIT_HASH={}", commit_hash);

    // 5. Trigger rebuild condition
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
