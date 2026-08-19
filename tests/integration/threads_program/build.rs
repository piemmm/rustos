//! Build script: select the freestanding build of the thread fixture and, for
//! the `tls` role, the one instruction that reads through the psABI thread
//! pointer.
//!
//! `freestanding` compiles `src/main.rs` as a real user-mode program on a
//! bare-metal target and leaves it an inert host stub everywhere else (mirrors
//! `mem_map_program/build.rs`).
//!
//! `threads_tp_<arch>` selects the thread-pointer read. Loading a `u64` at an
//! offset from the thread pointer is the psABI's own thread-local access and has
//! no architecture-neutral spelling: the register is `TPIDR_EL0` on aarch64,
//! `tp` on riscv64, and the `FS` segment base on x86_64 (which user code can
//! address *through* but, with `CR4.FSGSBASE` off, cannot read). The
//! instruction-set decision therefore lives here, in build glue, exactly as
//! `tp_probe_program/build.rs` and `lib/rt/build.rs` confine theirs — so
//! `cargo xtask cfg-check` stays clean and the choice is auditable in one place.

fn main() {
    for name in [
        "freestanding",
        "threads_tp_x86_64",
        "threads_tp_aarch64",
        "threads_tp_riscv64",
    ] {
        println!("cargo:rustc-check-cfg=cfg({name})");
    }
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "none" {
        println!("cargo:rustc-cfg=freestanding");
        match arch.as_str() {
            "x86_64" => println!("cargo:rustc-cfg=threads_tp_x86_64"),
            "aarch64" => println!("cargo:rustc-cfg=threads_tp_aarch64"),
            "riscv64" => println!("cargo:rustc-cfg=threads_tp_riscv64"),
            _ => {}
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
