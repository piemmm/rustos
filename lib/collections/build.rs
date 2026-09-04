//! Build script for the `tairix-collections` crate.
//!
//! Sole responsibility, and it is build glue (which is where a
//! target-conditional decision is allowed to live; `cargo xtask cfg-check`
//! enforces that the crate source carries none): decide which
//! per-architecture control-group scan candidate the hash table compiles in,
//! mirroring `lib/crc32c/build.rs` and `lib/abi-trap/build.rs`.
//!
//! Unlike those two, the gate is on the target *feature* as well as the
//! architecture. The scan candidates are vector-register code, and
//! `x86_64-unknown-none` is a soft-float, SSE-disabled kernel target whose
//! codegen backend cannot lower SSE intrinsics at all (see the
//! `chacha20_force_soft` pin in `.cargo/config.toml`) — and a kernel that has
//! not enabled the vector unit must not touch it in any case. So a candidate
//! is compiled only where the vector extension is already part of the target's
//! own feature set; every other target has none and runs the portable
//! baseline, which is always correct.
//!
//! The candidate is still only *selected* when the delivered `CpuFeatureSet`
//! reports the extension present and it reproduces the portable reference
//! bit-for-bit.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(swiss_sse2)");
    println!("cargo:rustc-check-cfg=cfg(swiss_neon)");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let has = |name: &str| features.split(',').any(|f| f == name);

    match arch.as_str() {
        "x86_64" if has("sse2") => println!("cargo:rustc-cfg=swiss_sse2"),
        "aarch64" if has("neon") => println!("cargo:rustc-cfg=swiss_neon"),
        _ => {}
    }
}
