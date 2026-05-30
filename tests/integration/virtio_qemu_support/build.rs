//! Build script: emit the shared `freestanding`/`itest_*` flags so this
//! support library selects its bring-up modules without naming the target
//! instruction set in its source (`AGENTS.md` §17.2). The flags are
//! produced by `rustos-itest-harness`, the single audited place that maps
//! a cargo target onto those names; every virtio QEMU vertical that links
//! this crate uses the same mapping (`AGENTS.md` §2.2 — no duplication).

fn main() {
    rustos_itest_harness::emit_target_cfg();
}
