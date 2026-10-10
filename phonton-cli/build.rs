// `managed_runtime`: targets with a pinned official Ollama archive that
// Phonton can install, start and verify (see src/provision.rs).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(managed_runtime)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if matches!(
        (os.as_str(), arch.as_str()),
        ("windows", "x86_64") | ("linux", "x86_64" | "aarch64") | ("macos", "x86_64" | "aarch64")
    ) {
        println!("cargo::rustc-cfg=managed_runtime");
    }
}
