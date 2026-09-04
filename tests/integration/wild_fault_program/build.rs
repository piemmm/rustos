//! Build script: enable the `wild_fault_x86_64` cfg when the ring-3
//! wild-fault fixture is built for the freestanding x86_64 target, so
//! `src/main.rs` compiles as a real user program there and as an inert host
//! stub everywhere else.
//!
//! Unlike the portable fixtures (`el0_yielder_program`, `mem_map_program`)
//! two of this one's roles must raise a *specific architectural exception* —
//! an invalid opcode and a privileged instruction — and neither has an
//! architecture-neutral spelling: Rust emits a checked panic rather than a
//! trapping instruction for every safe construct that might fault. The
//! instruction-set decision therefore lives here, in build glue, exactly as
//! `tests/integration/tp_probe_program/build.rs` confines its thread-pointer
//! write and `lib/abi-trap/build.rs` its per-target trap — so `cargo xtask
//! cfg-check` stays clean and the choice is auditable in one place.
//!
//! It is deliberately self-contained: it does not depend on the
//! `tests/integration` harness crate.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(wild_fault_x86_64)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "none" && arch == "x86_64" {
        println!("cargo:rustc-cfg=wild_fault_x86_64");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
