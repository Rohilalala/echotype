/// Every app command. With an app manifest, a window can call only the commands its capability
/// file allows: capabilities/default.json grants these to the settings window, and the overlay
/// gets only the pill_* ones. lib.rs tests keep this list, generate_handler! and the capability files in sync.
const COMMANDS: &[&str] = &[
    "get_settings",
    "save_settings",
    "get_presets",
    "set_secret",
    "has_secret",
    "delete_secret",
    "list_models",
    "download_model",
    "delete_model",
    "list_input_devices",
    "get_history",
    "clear_history",
    "get_stats",
    "clear_stats",
    "transcribe_file",
    "get_system_info",
    "local_api_status",
    "toggle_dictation",
    "shortcut_status",
    "relaunch",
    "retry_last",
    "check_permissions",
    "open_permission_settings",
    "cloud_start_login",
    "cloud_verify",
    "cloud_account",
    "cloud_checkout",
    "cloud_portal",
    "cloud_logout",
    "pill_click",
    "pill_cancel",
    "pill_open",
    "pill_menu",
    "pill_drag",
    "pill_reset",
    "pill_copy",
    "preview_sounds",
    "show_pill_demo",
    "pill_dismiss",
    "pill_offer",
    "meeting_start",
    "meeting_stop",
    "meeting_status",
    "meeting_list",
    "meeting_get",
    "meeting_rename_speaker",
    "meeting_delete",
    "meeting_export",
    "meeting_save",
];

fn main() {
    // whisper.cpp's Metal code uses @available checks, which clang lowers to
    // ___isPlatformVersionAtLeast from compiler-rt. rustc links with
    // -nodefaultlibs, so pull libclang_rt.osx.a in explicitly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let out = std::process::Command::new("xcrun")
            .args(["clang", "--print-resource-dir"])
            .output()
            .expect("xcrun clang --print-resource-dir");
        let dir = String::from_utf8(out.stdout).unwrap();
        println!("cargo:rustc-link-search=native={}/lib/darwin", dir.trim());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
        // screencapturekit's Swift bridge needs the OS Swift runtime; its own rpath line only
        // reaches its own targets, so every Echotype binary (and test) gets it here.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(COMMANDS))).expect("tauri-build failed");
}
