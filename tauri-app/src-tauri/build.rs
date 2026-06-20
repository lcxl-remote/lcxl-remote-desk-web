fn main() {
    tauri_build::build();
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
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
