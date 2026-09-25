// Entry point. Two modes, one binary.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Chrome launches this same executable as a Native Messaging host, passing the calling
    // extension's origin as an argument. In that mode it must be a silent stdio relay: no
    // window, no tray, no Tauri runtime. Shipping one binary means there is no helper app for
    // the user to notice, install, or have quarantined.
    if std::env::args().any(|a| a == "--native-messaging-host") {
        hvtt_desktop::bridge::run_native_messaging_host();
    }

    hvtt_desktop::run()
}
