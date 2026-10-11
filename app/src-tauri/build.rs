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
    }
    tauri_build::build()
}
