use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // 1. 获取 Git Version
    let output = Command::new("git")
        .args(&["rev-list", "--count", "HEAD"])
        .output();
    let build_number = match output {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout).unwrap().trim().to_string(),
        _ => "0".to_string(),
    };

    // 2. 将 build number 生成为一个完整的 Rust 代码文件
    //    以便在 version.rs 中完全作为一个模块 include 进来，对 IDE 更友好
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("built_build_number.rs");

    // 写入完整的 pub const 定义，包含文档注释
    let content = format!(
        "/// The current build number (Git revision count).\n/// This constant is of type i32, suitable for direct integer arithmetic.\npub const SERVER_BUILD_NUMBER: i32 = {};",
        build_number
    );
    fs::write(&dest_path, content).unwrap();

    // 3. 获取 Git Commit Hash
    let output_hash = Command::new("git").args(&["rev-parse", "HEAD"]).output();
    let commit_hash = match output_hash {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout).unwrap().trim().to_string(),
        _ => "unknown".to_string(),
    };

    // 4. 将 commit hash 设为环境变量 (rustc-env)
    println!("cargo:rustc-env=SERVER_COMMIT_HASH={}", commit_hash);

    // 5. 触发重构条件
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
