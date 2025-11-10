use std::env;
use std::path::Path;

pub fn main() {
    println!("cargo:rerun-if-env-changed=VPX_VERSION");
    println!("cargo:rerun-if-env-changed=VPX_LIB_DIR");
    println!("cargo:rerun-if-env-changed=VPX_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=VPX_STATIC");
    println!("cargo:rerun-if-env-changed=VPX_DYNAMIC");
    println!("cargo:rerun-if-changed=build.rs");

    #[allow(unused_assignments)]
    #[allow(unused_mut)]
    let mut requested_version = env::var("VPX_VERSION").ok();

    let src_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let src_dir = Path::new(&src_dir);

    let ffi_header = src_dir.join("ffi.h");
    let ffi_rs = {
        let out_dir = env::var_os("OUT_DIR").unwrap();
        let out_dir = Path::new(&out_dir);
        out_dir.join("ffi.rs")
    };

    #[allow(unused_assignments)]
    #[allow(unused_mut)]
    let mut vpx_lib_dir = env::var_os("VPX_LIB_DIR");
    #[cfg(target_os = "windows")]
    {
        let donwload_url;
        // On Windows, if the user specified a version, we append it to the lib dir.
        #[cfg(feature = "vpx_1_13")]
        {
            donwload_url = "https://github.com/ShiftMediaProject/libvpx/releases/download/v1.13.0/libvpx_v1.13.0_msvc16.zip";
            requested_version = Some("1.13.0".to_string());
        }

        #[cfg(feature = "vpx_1_12")]
        {
            donwload_url = "https://github.com/ShiftMediaProject/libvpx/releases/download/v1.12.0/libvpx_v1.12.0_msvc16.zip";
            requested_version = Some("1.12.0".to_string());
        }

        // If VPX_LIB_DIR is not set, we download a prebuilt libvpx and use that.
        let out_dir = env::var_os("OUT_DIR").unwrap();
        let out_dir = Path::new(&out_dir);
        let download_dir = out_dir.join("libvpx-download");
        let download_stamp = out_dir.join("libvpx-download.stamp");
        if !download_stamp.exists() {
            if download_dir.exists() {
                std::fs::remove_dir_all(&download_dir).unwrap();
            }
            std::fs::create_dir_all(&download_dir).unwrap();

            let zip_path = download_dir.join("libvpx.zip");
            {
                let mut resp = reqwest::blocking::get(donwload_url).unwrap();
                assert!(resp.status().is_success());
                let mut out = std::fs::File::create(&zip_path).unwrap();
                std::io::copy(&mut resp, &mut out).unwrap();
            }

            let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap())
                .expect("Failed to read libvpx zip file");
            zip.extract(&download_dir)
                .expect("Failed to extract libvpx zip file");

            std::fs::write(&download_stamp, b"downloaded")
                .expect("Failed to write libvpx download stamp");
        }

        // The extracted folder name is known from the zip contents.
        let libdir = download_dir.join("lib").join("x64");
        assert!(libdir.exists(), "Downloaded libvpx lib dir not found");
        vpx_lib_dir = Some(libdir.into());
    }
    #[allow(unused_variables)]
    let (found_version, include_paths) = match vpx_lib_dir {
        None => {
            // use VPX config from pkg-config
            let lib = pkg_config::probe_library("vpx").unwrap();

            if let Some(v) = requested_version {
                if lib.version != v {
                    panic!(
                        "version mismatch. pkg-config returns version {}, but VPX_VERSION \
                    environment variable is {}.",
                        lib.version, v
                    );
                }
            }
            (lib.version, lib.include_paths)
        }
        Some(vpx_libdir) => {
            // use VPX config from environment variable
            let libdir = std::path::Path::new(&vpx_libdir);

            // Set lib search path.
            println!("cargo:rustc-link-search=native={}", libdir.display());

            // Get static using pkg-config-rs rules.
            let statik = infer_static("VPX");

            // Set libname.
            // windows is always static link for now
            #[cfg(target_os = "windows")]
            println!("cargo:rustc-link-lib=static=libvpx");

            #[cfg(not(target_os = "windows"))]
            {
                if statik {
                    println!("cargo:rustc-link-lib=static=vpx");
                } else {
                    println!("cargo:rustc-link-lib=vpx");
                }
            }

            let mut include_paths = vec![];
            if let Some(include_dir) = env::var_os("VPX_INCLUDE_DIR") {
                include_paths.push(include_dir.into());
            }
            let version = requested_version.unwrap_or_else(|| {
                panic!("If VPX_LIB_DIR is set, VPX_VERSION must also be defined.")
            });
            (version, include_paths)
        }
    };

    println!("rerun-if-changed={}", ffi_header.display());
    for dir in &include_paths {
        println!("rerun-if-changed={}", dir.display());
    }

    #[cfg(feature = "generate")]
    generate_bindings(&ffi_header, &include_paths, &ffi_rs);

    #[cfg(not(feature = "generate"))]
    {
        let src = format!("vpx-ffi-{}.rs", found_version);
        let full_src = std::path::PathBuf::from("generated").join(src);
        if !full_src.exists() {
            panic!(
                "Expected file \"{}\" not found but 'generate' cargo feature not used.",
                full_src.display()
            );
        }
        std::fs::copy(&full_src, &ffi_rs).unwrap();
    }
}

// This function was modified from pkg-config-rs and should have same behavior.
fn infer_static(name: &str) -> bool {
    if env::var_os(&format!("{}_STATIC", name)).is_some() {
        true
    } else if env::var_os(&format!("{}_DYNAMIC", name)).is_some() {
        false
    } else if env::var_os("PKG_CONFIG_ALL_STATIC").is_some() {
        true
    } else if env::var_os("PKG_CONFIG_ALL_DYNAMIC").is_some() {
        false
    } else {
        false
    }
}

#[cfg(feature = "generate")]
fn generate_bindings(ffi_header: &Path, include_paths: &[std::path::PathBuf], ffi_rs: &Path) {
    let mut b = bindgen::builder()
        .header(ffi_header.to_str().unwrap())
        .allowlist_type("^[vV].*")
        .allowlist_var("^[vV].*")
        .allowlist_function("^[vV].*")
        .rustified_enum("^v.*")
        .trust_clang_mangling(false)
        .layout_tests(false) // breaks 32/64-bit compat
        .generate_comments(false); // vpx comments have prefix /*!\

    for dir in include_paths {
        b = b.clang_arg(format!("-I{}", dir.display()));
    }

    b.generate().unwrap().write_to_file(ffi_rs).unwrap();
}
