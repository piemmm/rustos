//! Build script: the shared x86_64 vertical build, with the boot stack the
//! aarch64 and riscv64 `virt` layouts give every image, because the whole
//! figure grid is drawn on the boot stack.

fn main() {
    tairix_itest_harness::x86_64_guest_build_with_virt_boot_stack();
}
