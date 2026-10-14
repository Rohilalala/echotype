// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--transcribe" || a == "--meeting") {
        // ponytail: on Windows release builds the GUI subsystem hides this output; AttachConsole if Windows CLI use matters.
        std::process::exit(echotype_lib::cli(&args));
    }
    // Wayland lets no app place its windows or keep them above others, so the pill runs under
    // XWayland when it is there. ponytail: the whole app goes through XWayland (blurry at
    // fractional scales); a layer-shell pill is the native upgrade.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_some() && std::env::var_os("GDK_BACKEND").is_none() {
        // SAFETY: still single-threaded; nothing has read the environment yet.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    }
    echotype_lib::run()
}
