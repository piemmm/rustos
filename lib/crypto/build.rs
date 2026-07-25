//! Build script for the `tairix-crypto` crate.
//!
//! Sole responsibility, and it is build glue (a build script is build glue, so
//! confining a target-conditional decision here keeps it out of the crate
//! source): decide, per compilation target, whether the audited SHA-256 crate
//! (`sha2`) genuinely provides a *hardware* backend that is selected at runtime
//! on a freestanding target, so `backend::resolve` may offer a hardware
//! candidate whose availability record matches what actually runs.
//!
//! The crate cannot make that decision inline with a target-architecture `cfg`
//! predicate — the charter forbids that outside the architecture ports, and
//! `cargo xtask cfg-check` enforces it. Instead this script reads the target
//! Cargo is building for and emits `crypto_hw_sha256` only on `x86_64`, where
//! `sha2`'s SHA-NI backend autodetects through `CPUID` — a detection that
//! works with no operating system, so it is correct on `x86_64-unknown-none`.
//! On `aarch64` the audited crate's hardware path is gated by `HWCAP`
//! detection that returns nothing without an OS (`target_os = "none"`), so it
//! silently stays on software; offering a hardware candidate there would
//! record a backend that does not run, so this script deliberately does not.
//! Every other target (riscv64, wasm32) likewise has no runtime-selected
//! hardware SHA-256 path.
//!
//! The name is emitted for the architecture regardless of OS, so a host test
//! build on an `x86_64` developer machine compiles the hardware-candidate wiring
//! and exercises the boot-time known-answer self-test against it (the candidate
//! is still only *selected* when the delivered `CpuFeatureSet` reports SHA-NI
//! and its prerequisite SSE levels present). Mirrors `lib/crc32c/build.rs`.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(crypto_hw_sha256)");

    if std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
        == "x86_64"
    {
        println!("cargo:rustc-cfg=crypto_hw_sha256");
    }
}
