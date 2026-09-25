//! Build script: the shared x86_64 vertical build, with the boot stack the
//! aarch64 and riscv64 `virt` layouts give every image, because two whole
//! frames, figures and all, are drawn on the boot stack.

fn main() {
    tairix_itest_harness::x86_64_guest_build_with_virt_boot_stack();
}
