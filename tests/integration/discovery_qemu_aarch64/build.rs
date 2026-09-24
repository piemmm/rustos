//! Build script for the aarch64 live discovery QEMU vertical
//! (`plans/ZEROCONF.md` Z4): the shared aarch64 `virt` guest build.

fn main() {
    // One CPU: every boot service, the autoloaded driver, discoveryd and its
    // decoder, the login, and the scripted session timeshare on it.
    tairix_itest_harness::aarch64_virt_guest_build(1);
}
