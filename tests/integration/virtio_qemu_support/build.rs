//! Build script: emit the shared `freestanding`/`itest_*` flags so this
//! support library selects its bring-up modules without naming the target
//! instruction set in its source. The flags are
//! produced by `tairix-itest-harness`, the single audited place that maps
//! a cargo target onto those names; every virtio QEMU vertical that links
//! this crate uses the same mapping (no duplication).

fn main() {
    tairix_itest_harness::emit_target_cfg();
}
