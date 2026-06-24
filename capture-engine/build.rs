//! Emits the `av1_supported` cfg for targets where the SVT-AV1 backend can be
//! built. The `shiguredo_svt_av1` crate ships only prebuilt binaries (no source
//! build is enabled) for a fixed matrix — Linux x86_64/aarch64, macOS arm64,
//! Windows x86_64 — and its build script panics on any other target. The
//! dependency is target-gated in Cargo.toml to that matrix; this cfg lets the
//! code compile the AV1 encoder in/out to match, without repeating the long
//! target expression at every gate.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(av1_supported)");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let supported = matches!(
        (os.as_str(), arch.as_str()),
        ("linux", "x86_64") | ("linux", "aarch64") | ("macos", "aarch64") | ("windows", "x86_64")
    );
    if supported {
        println!("cargo::rustc-cfg=av1_supported");
    }
}
