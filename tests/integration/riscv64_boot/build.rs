//! Build script: emit the shared `freestanding`/`itest_*` flags so this
//! crate selects its boot-orchestration module without naming the target
//! instruction set in its source. The mapping lives
//! in `rustos-itest-harness`, the single audited place that maps a cargo
//! target onto those names (no duplication).

fn main() {
    rustos_itest_harness::emit_target_cfg();
}
