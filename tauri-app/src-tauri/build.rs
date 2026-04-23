fn main() {
    copy_desk_standalone_sidecar();
    tauri_build::build();
}

/// Copy the compiled `lcxl-remote-desk-server` binary into `src-tauri/binaries/` with
/// the Tauri-required target-triple suffix so the bundler picks it up.
fn copy_desk_standalone_sidecar() {
    let target_triple = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());

    // Locate the workspace target directory (4 levels up from src-tauri).
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"),
    );
    // manifest_dir = .../web/tauri-app/src-tauri
    // → web workspace root = 2 parents up (tauri-app → web)
    let workspace_root = manifest_dir
        .parent() // tauri-app
        .and_then(|p| p.parent()) // web (workspace root for lcxl-remote-desk-server)
        .expect("Could not determine workspace root");

    let mut src = workspace_root.join("target").join(&profile);
    #[cfg(target_os = "windows")]
    src.push("lcxl-remote-desk-server.exe");
    #[cfg(not(target_os = "windows"))]
    src.push("lcxl-remote-desk-server");

    let dest_dir = manifest_dir.join("binaries");
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        println!("cargo:warning=Failed to create binaries/ dir: {e}");
        return;
    }

    #[cfg(target_os = "windows")]
    let dest = dest_dir.join(format!("lcxl-remote-desk-server-{}.exe", target_triple));
    #[cfg(not(target_os = "windows"))]
    let dest = dest_dir.join(format!("lcxl-remote-desk-server-{}", target_triple));

    if src.exists() {
        match std::fs::copy(&src, &dest) {
            Ok(_) => println!(
                "cargo:warning=Sidecar copied: {} → {}",
                src.display(),
                dest.display()
            ),
            Err(e) => println!("cargo:warning=Failed to copy lcxl-remote-desk-server sidecar: {e}"),
        }
    } else {
        println!(
            "cargo:warning=lcxl-remote-desk-server not found at {} (build it first)",
            src.display()
        );
    }

    // Rebuild if the source binary changes.
    println!("cargo:rerun-if-changed={}", src.display());
}
