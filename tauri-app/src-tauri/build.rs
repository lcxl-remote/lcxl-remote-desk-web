fn main() {
    copy_desk_standalone_sidecar();
    tauri_build::build();
}

/// Copy the compiled `desk-standalone` binary into `src-tauri/binaries/` with
/// the Tauri-required target-triple suffix so the bundler picks it up.
fn copy_desk_standalone_sidecar() {
    let target_triple = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());

    // Locate the workspace target directory (4 levels up from src-tauri).
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"),
    );
    // manifest_dir = .../web/tauri-app/src-tauri
    // → workspace root = 3 parents up
    let workspace_root = manifest_dir
        .parent()  // tauri-app
        .and_then(|p| p.parent())  // web
        .and_then(|p| p.parent())  // workspace root
        .expect("Could not determine workspace root");

    let mut src = workspace_root.join("target").join(&profile);
    #[cfg(target_os = "windows")]
    src.push("desk-standalone.exe");
    #[cfg(not(target_os = "windows"))]
    src.push("desk-standalone");

    let dest_dir = manifest_dir.join("binaries");
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        println!("cargo:warning=Failed to create binaries/ dir: {e}");
        return;
    }

    #[cfg(target_os = "windows")]
    let dest = dest_dir.join(format!("desk-standalone-{}.exe", target_triple));
    #[cfg(not(target_os = "windows"))]
    let dest = dest_dir.join(format!("desk-standalone-{}", target_triple));

    if src.exists() {
        match std::fs::copy(&src, &dest) {
            Ok(_) => println!(
                "cargo:warning=Sidecar copied: {} → {}",
                src.display(),
                dest.display()
            ),
            Err(e) => println!(
                "cargo:warning=Failed to copy desk-standalone sidecar: {e}"
            ),
        }
    } else {
        println!(
            "cargo:warning=desk-standalone not found at {} (build it first)",
            src.display()
        );
    }

    // Rebuild if the source binary changes.
    println!("cargo:rerun-if-changed={}", src.display());
}
