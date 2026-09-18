//! Records the toolchain barry is built with, for the review prompt.
//!
//! A local model's training ends months before it is used, so today's date
//! reads as the future and the current Rust edition as a typo: the 35B on
//! 2026-09-18 filed four findings against comments that mentioned that day.
//! The prompt therefore states the date and the toolchain. The toolchain is
//! captured here rather than typed in, so it moves when the build moves.

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "rustc (version unknown)".to_string());
    println!("cargo:rustc-env=BARRY_RUSTC_VERSION={version}");

    // The edition this crate compiles with is the newest one this toolchain
    // knows; that is the one to tell the model exists.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest =
        std::fs::read_to_string(format!("{manifest_dir}/Cargo.toml")).unwrap_or_default();
    let edition = manifest
        .lines()
        .find_map(|l| l.trim().strip_prefix("edition = \""))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or("2024");
    println!("cargo:rustc-env=BARRY_RUST_EDITION={edition}");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=RUSTC");
}
