fn main() {
    println!("cargo::rustc-check-cfg=cfg(hip_enabled)");
    if cfg!(feature = "hip")
        && matches!(
            std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").as_deref(),
            Ok("64")
        )
        && matches!(
            std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
            Ok("x86_64" | "aarch64")
        )
        && matches!(
            std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
            Ok("windows" | "linux")
        )
    {
        println!("cargo::rustc-cfg=hip_enabled");
    }

    gst_plugin_version_helper::info()
}
