fn main() {
    // Windows: Vulkan's loader (vulkan-1.dll) is asked for when first used, not when the program
    // starts - so a PC without it still opens, and `engine_whisper::vulkan_ready` decides where
    // it comes from (the graphics driver's copy, or the one beside the program).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:vulkan-1.dll");
        println!("cargo:rustc-link-lib=delayimp");
        // A test or example that never reaches the engine imports nothing from it: not worth a warning.
        println!("cargo:rustc-link-arg=/IGNORE:4199");
    }
    tauri_build::build()
}
