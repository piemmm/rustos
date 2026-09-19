//! Build script: register the `loom` cfg flag so `rustc`'s
//! `unexpected_cfgs` check is happy.
//!
//! `loom` is a *cfg flag*, not a Cargo feature, because enabling it pulls in
//! the `loom` crate (which needs `std`) and swaps the PCM ring's atomics for
//! the model checker's — a change that is not a stable, additive feature in
//! the Cargo sense. It is enabled by `RUSTFLAGS="--cfg loom"`, which is what
//! `cargo xtask loom` passes. See `tests/loom.rs`.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(loom)");
}
