// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--transcribe" || a == "--meeting") {
        // ponytail: on Windows release builds the GUI subsystem hides this output; AttachConsole if Windows CLI use matters.
        std::process::exit(echotype_lib::cli(&args));
    }
    echotype_lib::run()
}
