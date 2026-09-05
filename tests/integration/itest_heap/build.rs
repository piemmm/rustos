//! Build script: emit the shared `freestanding`/`itest_*` classification so
//! the allocator compiles only into a bare-metal image. On a host build the
//! crate is inert, which is what keeps its own test binary on the standard
//! library's allocator rather than a one-page arena.

fn main() {
    tairix_itest_harness::emit_target_cfg();
}
