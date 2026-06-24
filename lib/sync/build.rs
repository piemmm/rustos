//! Build script: register the `loom` cfg flag so `rustc`'s
//! `unexpected_cfgs` check is happy.
//!
//! `loom` is a *cfg flag*, not a Cargo feature, because enabling it
//! pulls in the `loom` crate (which depends on `std`) and rewires every
//! atomic in `kernel/sync` to go through the model checker — a change
//! that is not a stable, additive feature in the Cargo sense. It is
//! enabled by passing `RUSTFLAGS="--cfg loom"`. See and
//! `tests/loom.rs`.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(loom)");
}
