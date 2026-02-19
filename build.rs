fn main() {
    // Set COMMIT_ID and BUILD_REL_DATE for gst::plugin_define!
    // gst_plugin_version_helper needs git which isn't available in cross Docker.
    let commit_id = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let build_date = std::process::Command::new("git")
        .args(["log", "-1", "--format=%cd", "--date=short"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "2025-01-01".to_string());

    println!("cargo:rustc-env=COMMIT_ID={}", commit_id);
    println!("cargo:rustc-env=BUILD_REL_DATE={}", build_date);

    // When cross-compiling, use the MPP headers/lib bundled in mpp-aarch64/
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let mpp_lib_dir = format!("{}/mpp-aarch64/lib", manifest_dir);

    if std::path::Path::new(&mpp_lib_dir).exists() {
        println!("cargo:rustc-link-search=native={}", mpp_lib_dir);
    }

    println!("cargo:rustc-link-lib=dylib=rockchip_mpp");
}
