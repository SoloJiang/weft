fn main() {
    tauri_build::build();

    // On Windows, `cargo test` harness binaries do NOT inherit the application
    // manifest that tauri-build embeds into the app binary, so they lack the
    // Common-Controls v6 dependency. `rfd` (native dialogs) statically imports
    // comctl32!TaskDialogIndirect — a v6-only export absent from System32's
    // v5.82 comctl32 — so an unmanifested test exe fails to load with
    // STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139) before any test runs. Embed the
    // manifest for test targets only; the app bin already has tauri's.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
            let manifest = std::path::Path::new(&dir).join("tests.manifest");
            println!("cargo:rerun-if-changed=tests.manifest");
            println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
                manifest.display()
            );
        }
    }
}
