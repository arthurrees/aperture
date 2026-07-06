// Build script: on the GNU toolchain, webview2-com-sys does NOT copy WebView2Loader.dll next to our
// binary (that auto-copy is MSVC-only), but the exe needs it at runtime. webview2-com-sys extracts
// the loader into its own build output dir; copy the x64 loader into target/<profile> so the binary
// finds it next to itself.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Embed the application icon (drives the .exe / Explorer / taskbar icon, and is loaded as the
    // window icon at runtime). Uses windres on the GNU toolchain (WinLibs must be on PATH).
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let _ = embed_resource::compile("app.rc", embed_resource::NONE);

    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };
    let out_dir = PathBuf::from(out_dir); // .../target/<profile>/build/<our-pkg>/out
                                          // target/<profile> is 3 ancestors up: out -> <pkg> -> build -> <profile>
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    let build_dir = profile_dir.join("build");
    let Ok(entries) = fs::read_dir(&build_dir) else {
        return;
    };
    for e in entries.flatten() {
        if !e
            .file_name()
            .to_string_lossy()
            .starts_with("webview2-com-sys-")
        {
            continue;
        }
        let loader = e.path().join("out").join("x64").join("WebView2Loader.dll");
        if loader.exists() {
            let dest = profile_dir.join("WebView2Loader.dll");
            if fs::copy(&loader, &dest).is_ok() {
                println!(
                    "cargo:warning=copied WebView2Loader.dll -> {}",
                    dest.display()
                );
            }
            break;
        }
    }
}
