//! Exports the minimum Rust version `vxedge doctor` enforces, read from the workspace's
//! `rust-toolchain.toml` so the check and the pin cannot drift apart. Falls back to the crate's
//! `rust-version` when the file is absent or its channel is not a plain version (`stable`, ...).

use std::path::PathBuf;

fn pinned_channel() -> Option<String> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let file = manifest.join("../../rust-toolchain.toml");
    println!("cargo:rerun-if-changed={}", file.display());
    let text = std::fs::read_to_string(file).ok()?;
    let channel = text
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("channel"))?
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .trim_matches('"')
        .to_string();
    let numeric = !channel.is_empty()
        && channel.split('.').count() >= 2
        && channel
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    numeric.then_some(channel)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let min = pinned_channel()
        .or_else(|| std::env::var("CARGO_PKG_RUST_VERSION").ok())
        .expect("rust-toolchain.toml channel or package rust-version must be set");
    println!("cargo:rustc-env=VXEDGE_MIN_RUSTC={min}");
}
