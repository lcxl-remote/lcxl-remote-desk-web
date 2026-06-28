use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // 1. Get Git Version
    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
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

    // Only write if content changed to avoid unnecessary recompilation
    let existing_content = fs::read_to_string(&dest_path).ok();
    if existing_content.as_deref() != Some(&content) {
        fs::write(&dest_path, content).unwrap();
    }

    // 3. Get Git Commit Hash
    let output_hash = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let commit_hash = match output_hash {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout).unwrap().trim().to_string(),
        _ => "unknown".to_string(),
    };

    // 4. Set commit hash as environment variable (rustc-env)
    println!("cargo:rustc-env=SERVER_COMMIT_HASH={}", commit_hash);

    // 4b. Resolve the source repository URL. A release/CI build can pin a clean,
    //     public URL via the `SERVER_REPOSITORY_URL` env var (which also avoids
    //     embedding a private remote such as an internal SSH host); otherwise it
    //     falls back to the local `origin` remote, then to empty when unknown.
    let repository_url = match env::var("SERVER_REPOSITORY_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().to_string(),
        _ => {
            let output_remote = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .output();
            match output_remote {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => String::new(),
            }
        }
    };
    println!("cargo:rerun-if-env-changed=SERVER_REPOSITORY_URL");
    println!("cargo:rustc-env=SERVER_REPOSITORY_URL={}", repository_url);

    // 5. Trigger rebuild condition
    // Dynamically resolve git directory to support submodules and different layouts
    let git_dir_output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok();

    if let Some(output) = git_dir_output
        && output.status.success()
    {
        let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
        println!("cargo:rerun-if-changed={}/HEAD", git_dir);
        println!("cargo:rerun-if-changed={}/index", git_dir);
    }

    emit_macos_swift_rpath();
}

/// The macOS capture backend pulls in `screencapturekit` / `apple-cf`, whose
/// Swift bridge makes the final binary depend on `@rpath/libswift_Concurrency.dylib`
/// (the Swift Concurrency back-deployment runtime). Those crates emit the rpath
/// from their own build scripts, but `cargo:rustc-link-arg` does not propagate to
/// downstream binaries, so the rpath must be added here. macOS 13+ ships the
/// runtime in `/usr/lib/swift` (resolved via the dyld shared cache), which works
/// both for local runs and for distributed apps.
fn emit_macos_swift_rpath() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
