//! Build script for the aarch64 live telnet QEMU vertical
//! (`plans/TELNET.md`): the shared aarch64 `virt` guest build.

fn main() {
    // One CPU: PID 1, the unlock kthread, netstack, devmgr, the autoloaded
    // driver process, the console login, and the spawned session all
    // timeshare on the boot CPU under preemptive scheduling.
    tairix_itest_harness::aarch64_virt_guest_build(1);
}
