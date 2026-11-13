// SPDX-License-Identifier: GPL-3.0-only
//! Compile the *production* `crates/app/ui/main.slint` unchanged.
//!
//! `MainWindow` is generated from the same markup the app uses. No app Rust
//! code, AppState, build flags or generated bindings are touched: the spike
//! only consumes the exported component and its callbacks.

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ui = manifest.join("../app/ui/main.slint");
    println!("cargo:rerun-if-changed={}", ui.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("../app/ui").display()
    );
    slint_build::compile(&ui).expect("compile crates/app/ui/main.slint for the spike");
}
