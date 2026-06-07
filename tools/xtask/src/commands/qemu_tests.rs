//! QEMU integration-test driver invoked by `cargo xtask test --qemu`.
//!
//! AGENTS.md §7 mandates that the QEMU tests share the same orchestrator
//! as host-side tests and that each QEMU run has a *strict* per-test
//! timeout with **no retries**. This module enforces both: it builds the
//! enrolled kernels for `x86_64-unknown-none`, then drives each one
//! through [`rustos_qemu::Runner::run`] in series, failing the whole
//! `xtask test` invocation on the first failure or timeout.

use std::path::PathBuf;
use std::time::Duration;

use rustos_qemu::{Outcome, Runner, Spec};

use crate::Context;

/// Per-test wall-clock ceiling enforced on a developer machine (a
/// `cargo xtask ci` / `test --qemu` run outside GitHub Actions). The
/// enrolled budgets (up to 120 s) are sized for the CI runners that carry
/// the flake-hunting budget; a developer running the matrix from the IDE
/// instead gets a 30 s ceiling per test, so a hung guest fails fast rather
/// than stalling the local run. The runners keep the full enrolled budget
/// (the same developer-vs-runner split as the 20× test repeat, §7).
const DEVELOPER_TIMEOUT_CAP: Duration = Duration::from_secs(30);

/// The wall-clock budget to enforce for an enrolment on this host: the
/// enrolment's own [`QemuTest::timeout`] on a CI runner, or that value
/// clamped to [`DEVELOPER_TIMEOUT_CAP`] on a developer machine. Lowering a
/// ceiling never extends a budget, so this can only make a local run fail
/// faster, never hide a slow CI run.
fn effective_timeout(timeout: Duration, in_github_actions: bool) -> Duration {
    if in_github_actions {
        timeout
    } else {
        timeout.min(DEVELOPER_TIMEOUT_CAP)
    }
}

/// One enrolled QEMU integration test.
struct QemuTest {
    /// Cargo package name (matches `[package].name`).
    package: &'static str,
    /// Binary name produced by the package (`[[bin]].name`).
    binary: &'static str,
    /// Rust target triple the binary is built for. Selects both the
    /// `cargo build --target` value and the per-arch QEMU `Spec`
    /// constructor (`x86_64-unknown-none` → `isa-debug-exit`;
    /// `riscv64gc-unknown-none-elf` → the `virt` board + `SiFive`
    /// Test finisher).
    target: &'static str,
    /// Number of emulated CPUs.
    cpus: u32,
    /// Hard wall-clock budget.
    timeout: Duration,
    /// When `Some(n)`, attach an `n`-sector raw virtio-blk backing
    /// image whose sector 0 carries the deterministic pattern
    /// `byte[i] = i mod 256` (which the kernel-side test verifies).
    disk_sectors: Option<u64>,
    /// When `true`, attach a QEMU user-mode (SLIRP) virtio-net interface
    /// and dump every frame to a `<binary>.pcap` capture beside the
    /// kernel image so a host can inspect the on-wire exchange.
    virtio_net: bool,
    /// When `true`, attach a QEMU `ramfb` display device (a
    /// firmware-programmed linear framebuffer in guest RAM). Used by the
    /// framebuffer-display vertical on the riscv64 `virt` board.
    ramfb: bool,
    /// Filesystem volume to plant on a raw virtio-blk backing image
    /// (independent of the `disk_sectors` sector-0 pattern). The
    /// kernel-side test mounts it through the real driver and
    /// round-trips a read and a write.
    fs_disk: FsDisk,
    /// When `Some((marker, key))`, attach a `virtio-keyboard-device` and
    /// inject `key` (a QEMU `QKeyCode`) once the guest prints `marker` on
    /// the serial console. Used by the aarch64 virtio-input vertical to
    /// make a real device→driver input event deterministic.
    keyboard: Option<(&'static str, &'static str)>,
}

/// Which filesystem volume (if any) the host harness plants on the
/// test's virtio-blk backing image. Each variant names a shared
/// single-source-of-truth image fixture (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Eq, PartialEq)]
enum FsDisk {
    /// No filesystem volume (the test uses `disk_sectors` or no disk).
    None,
    /// The shared [`rustos_test_fat32_image`] FAT32 volume.
    Fat32,
    /// The shared [`rustos_test_rustfs_image`] rustfs volume.
    Rustfs,
}

const TESTS: &[QemuTest] = &[
    QemuTest {
        package: "rustos-test-memory-isolation",
        binary: "rustos-test-memory-isolation",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3a (b) deliverable: AP bring-up + scheduler stress on real
    // (emulated) cores. The host-side `rustos-test-scheduler-stress`
    // workspace test continues to satisfy the AGENTS.md §7 unit / cross-
    // crate contract; this enrolment is the QEMU-on-real-cores half of
    // the same Stage-2 deliverable mandated by `PLAN.md` lines 154-158.
    QemuTest {
        package: "rustos-test-scheduler-stress-qemu",
        binary: "rustos-test-scheduler-stress-qemu",
        target: "x86_64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3a (c7-bin) deliverable: boot the production
    // `rustos-kernel` boot pipeline (Multiboot2 → ACPI/MADT →
    // `X86_64Arch` → per-CPU init → `BootInfo` →
    // `kernel_core::kernel_main`) and assert
    // `AuditEvent::BootCompleted` (`EventId(4004)`) appears on the
    // audit sink. The test binary `rustos-test-kernel-arch-boot`
    // wraps the lib half of `rustos-kernel` with an audit-observer
    // Sink that flips `qemu_exit::exit_success` on observing
    // `BootCompleted` — see
    // `tests/integration/kernel_arch_boot/src/main.rs`. Single CPU
    // suffices: the (c7-bin) scope only brings up the BSP. The
    // 60-second budget matches `memory_isolation`'s — both are
    // strictly bring-up tests with no workload.
    QemuTest {
        package: "rustos-test-kernel-arch-boot",
        binary: "rustos-test-kernel-arch-boot",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 2.7 follow-up (f6) deliverable: boot the production
    // `rustos-kernel` boot pipeline and, on observing
    // `AuditEvent::BootCompleted`, synthesise a Scheduler / CapTable /
    // KernelSyscallHandlers / Dispatcher quartet locally and drive
    // `Dispatcher::dispatch` with `(cap_query, CAP_TIME_SET)` then
    // `(exit, 0)`. The synthesised inner audit sink counts the
    // `SyscallInvoked` (`EventId(5000)`) record emitted by the
    // `exit` dispatch (the `cap_query` half is `audit: false` per
    // the abi-v1 table — observed via the dispatcher's return value
    // instead). The test bin flips `qemu_exit::exit_success` only
    // when both halves complete cleanly; anything else trips
    // `qemu_exit::exit_failure`. Single CPU suffices and the
    // 60-second budget matches `kernel_arch_boot`'s — same boot
    // pipeline plus a fixed-size dispatcher exercise.
    QemuTest {
        package: "rustos-test-syscall-dispatch-qemu",
        binary: "rustos-test-syscall-dispatch-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC2 deliverable (`plans/CCOMPAT.md`): the per-native-
    // target QEMU round-trip for the C-callable syscall stub runtime
    // (`lib/abi-sys`). Unlike `rustos-test-syscall-dispatch-qemu` (which
    // drives `Dispatcher::dispatch` directly and never executes a trap),
    // this test boots the production kernel pipeline and, on
    // `AuditEvent::BootCompleted`, overrides the syscall dispatch callback
    // and then *issues* the `abi-sys` `ros_sys_cap_query` stub — exercising
    // the real x86_64 `syscall` instruction (`lib/abi-sys/src/trap.rs`) and
    // the kernel's `IA32_LSTAR` entry stub
    // (`kernel/arch/x86_64/src/syscall_entry.rs`) together. The installed
    // callback asserts the kernel-observed `(number, args)` are exactly
    // what `ros_sys_cap_query` should have marshalled into the syscall
    // registers and flips `qemu_exit::exit_success`; any mismatch (or the
    // `syscall` returning to its caller at all) flips
    // `qemu_exit::exit_failure`. Single CPU suffices and the 60-second
    // budget matches the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-abi-sys-syscall-qemu",
        binary: "rustos-test-abi-sys-syscall-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC2 deliverable (`plans/CCOMPAT.md`): the riscv64
    // half of the `lib/abi-sys` syscall-stub round-trip. riscv64 has no
    // x86_64-style "trap identically from any privilege" shortcut — the
    // kernel routes only an `ecall` *from U-mode* to the syscall dispatch
    // callback (`kernel/arch/riscv64/src/syscall_entry.rs`) — so this test
    // stands up a minimal U-mode context with the Stage-3 Sv39 primitives:
    // it identity-maps the kernel (S-mode), aliases the `ros_sys_cap_query`
    // stub page at a user virtual address with the U bit set plus a user
    // stack, installs the dispatch callback, sets `sstatus.SUM`, and
    // `sret`s to U-mode. The stub's real `ecall` (`lib/abi-sys/src/trap.rs`)
    // then traps into the kernel S-mode trap vector, and the installed
    // callback asserts the kernel-observed `(number, args)` are exactly
    // what `ros_sys_cap_query` should have marshalled into `a7`/`a0` before
    // writing the `SiFive` Test PASS finisher; any mismatch (or the `ecall`
    // resuming in U-mode at all) writes a distinct failure finisher. Single
    // CPU suffices and the 60-second budget matches the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-abi-sys-syscall-qemu-riscv64",
        binary: "rustos-test-abi-sys-syscall-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC2 deliverable (`plans/CCOMPAT.md`): the aarch64
    // half of the `lib/abi-sys` syscall-stub round-trip. Like riscv64,
    // aarch64 has no x86_64-style "trap identically from any privilege"
    // shortcut — the kernel routes only an `svc` *from EL0* (a lower-EL
    // synchronous exception) to the syscall dispatch callback
    // (`kernel/arch/aarch64/src/exceptions.rs`) — so this test stands up a
    // minimal EL0 context with the Stage-3 stage-1 primitives: it
    // identity-maps the kernel (EL1), aliases the `ros_sys_cap_query` stub
    // page at a user virtual address with EL0-executable attributes plus
    // an EL0 stack, installs the dispatch callback and the EL1 vector
    // table, and `eret`s to EL0. The stub's real `svc`
    // (`lib/abi-sys/src/trap.rs`) then traps into the EL1 vector, and the
    // installed callback asserts the kernel-observed `(number, args)` are
    // exactly what `ros_sys_cap_query` should have marshalled into
    // `x8`/`x0` before the ARM semihosting PASS finisher; any mismatch (or
    // the `svc` resuming in EL0 at all) writes a distinct failure
    // finisher. Single CPU suffices and the 60-second budget matches the
    // other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-abi-sys-syscall-qemu-aarch64",
        binary: "rustos-test-abi-sys-syscall-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the x86_64
    // ring-3 exercise for the Arch HAL "enter user mode" primitive
    // (`kernel/arch/x86_64/src/userentry.rs`, `rustos_arch_api::EnterUser`,
    // `AGENTS.md` §17.2). Unlike `rustos-test-abi-sys-syscall-qemu`, which
    // issues the same `abi-sys` stub from ring 0 (the x86_64 `syscall`
    // traps identically from any privilege level and never crosses a
    // boundary), this test boots the production kernel and, on
    // `AuditEvent::BootCompleted`, builds a ring-3 address space — a
    // user-accessible, executable, non-writable alias of the
    // `ros_sys_cap_query` stub page (W^X, §19.2) plus a USER read/write
    // stack — switches CR3, and `iretq`s to ring 3 through
    // `UserMode::new().enter_user(...)`. The stub's real `syscall`
    // (`lib/abi-sys/src/trap.rs`) then traps back through the kernel's
    // `IA32_LSTAR` entry stub; reaching the installed dispatch callback
    // at all proves the `iretq` entry succeeded, and the callback asserts
    // the kernel-observed `(number, args)` are exactly what
    // `ros_sys_cap_query` should have marshalled into the syscall
    // registers before flipping `qemu_exit::exit_success`; any mismatch
    // flips `qemu_exit::exit_failure`. Single CPU suffices and the
    // 60-second budget matches the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-enter-user-qemu-x86_64",
        binary: "rustos-test-enter-user-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the riscv64
    // crt0-linked-program spawn round-trip. The build script compiles the
    // separate fixture program (`tests/integration/cc3_program`, crt0 +
    // abi-sys) position-independent and converts it to an `rxe` blob
    // (`rustos_itest_harness::elf2rxe`) carrying the kernel's syscall CFI tag.
    // On boot the test stands up an Sv39 address space (identity-mapping the
    // kernel + MMIO), activates it, installs the trap vector and a dispatch
    // callback, then calls the production capability-checked, audited spawn
    // caller (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`)
    // to build the program's U-mode image — segments mapped + filled, user
    // stack, startup-vector block — and `sret`s into it through the Arch HAL
    // `EnterUser` primitive. The program (built via `build_process_image` at a
    // high `USER_BIAS`) parses `argv[1]`, returns it, and crt0 routes the
    // return through the `exit` syscall, whose `ecall` traps back through the
    // kernel S-mode vector to the dispatch callback, which asserts the code
    // equals the spawned decimal argument before the `SiFive` Test PASS
    // finisher; any mismatch (or a returning spawn) writes a distinct failure
    // finisher. Single CPU suffices and the 60-second budget matches the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-spawn-program-qemu-riscv64",
        binary: "rustos-test-spawn-program-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the aarch64
    // crt0-linked-program spawn round-trip — the EL0 analogue of the riscv64
    // test above. The build script compiles the separate fixture program
    // (`tests/integration/cc3_program`, crt0 + abi-sys) position-independent
    // and converts it to an `rxe` blob (`rustos_itest_harness::elf2rxe`)
    // carrying the kernel's syscall CFI tag. On boot the test stands up a
    // stage-1 address space (identity-mapping the kernel + MMIO, EL1),
    // activates it, installs the EL1 vector table and a dispatch callback, then
    // calls the production capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's EL0 image — segments mapped + filled, user stack,
    // startup-vector block — and `eret`s into it through the Arch HAL
    // `EnterUser` primitive. The program (built via `build_process_image` at a
    // high `USER_BIAS`) parses `argv[1]`, returns it, and crt0 routes the
    // return through the `exit` syscall, whose `svc` traps back through the
    // kernel EL1 vector to the dispatch callback, which asserts the code equals
    // the spawned decimal argument before the ARM semihosting PASS finisher;
    // any mismatch (or a returning spawn) writes a distinct failure finisher.
    // Single CPU suffices and the 60-second budget matches the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-spawn-program-qemu-aarch64",
        binary: "rustos-test-spawn-program-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the x86_64
    // crt0-linked-program spawn round-trip — the ring-3 analogue of the
    // riscv64/aarch64 tests above, completing CC3. The build script compiles
    // the separate fixture program (`tests/integration/cc3_program`, crt0 +
    // abi-sys) position-independent and converts it to an `rxe` blob
    // (`rustos_itest_harness::elf2rxe`) carrying the kernel's syscall CFI tag.
    // Because the x86_64 ring-3 transition needs the GDT user selectors, the
    // TSS, and `syscall`/`IA32_LSTAR` entry installed, the test boots the
    // production kernel pipeline and, on `AuditEvent::BootCompleted`, enables
    // `IA32_EFER.NXE`, builds a fresh address space (low 32 MiB identity +
    // higher-half kernel window), switches CR3, installs a dispatch callback,
    // then calls the production capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's ring-3 image — segments mapped + filled W^X (code RX,
    // data RW-NX, rodata R-NX), user stack, startup-vector block — and `iretq`s
    // into it through the Arch HAL `EnterUser` primitive. The program (built via
    // `build_process_image` at a high `USER_BIAS`) parses `argv[1]`, returns it,
    // and crt0 routes the return through the `exit` syscall, whose `syscall`
    // traps back through the kernel's `IA32_LSTAR` entry stub to the dispatch
    // callback, which asserts the code equals the spawned decimal argument
    // before `qemu_exit::exit_success`; any mismatch (or a returning spawn)
    // flips `qemu_exit::exit_failure`. Single CPU suffices and the 60-second
    // budget matches the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-spawn-program-qemu-x86_64",
        binary: "rustos-test-spawn-program-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the riscv64
    // end-to-end C-program round-trip — the headline CC5 work. The build
    // script builds the Rust crt0 + `ros_sys_*` runtime shim
    // (`tests/integration/cc5_program`) as a PIE `staticlib`, compiles the
    // genuinely C-language program (`cc5_program/csrc/main.c`) with the audited,
    // version-pinned, checksummed `clang`/`ld.lld` wrapper (`tools/cc`,
    // AGENTS.md §12), links them into one PIE image, and converts it to an `rxe`
    // blob (`rustos_itest_harness::elf2rxe`) carrying the kernel's syscall CFI
    // tag. On boot the test stands up an Sv39 address space (identity-mapping
    // the kernel + MMIO), installs the trap vector and a dispatch callback, then
    // calls the production capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to build
    // the program's U-mode image and `sret` into it. The C program checks a
    // Time64 value across the §21 pre-1970/post-2038 boundaries, an ipc header,
    // and a sysinfo header, then issues `cap_query` + `clock_get`; the callback
    // services those (asserting the marshalled cap id, returning a 64-bit
    // sentinel) and asserts the `exit` code is 99 before the `SiFive` Test PASS
    // finisher. Proves the generated C header, the `ros_sys_*` runtime, and crt0
    // agree with the Rust side end to end. Single CPU; 60-second run budget.
    QemuTest {
        package: "rustos-test-c-program-qemu-riscv64",
        binary: "rustos-test-c-program-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the aarch64
    // end-to-end C-program round-trip — the EL0 analogue of the riscv64
    // vertical above. The build script builds the Rust crt0 + `ros_sys_*`
    // runtime shim (`tests/integration/cc5_program`) as a PIE `staticlib`,
    // compiles the genuinely C-language program (`cc5_program/csrc/main.c`)
    // with the audited, version-pinned, checksummed `clang`/`ld.lld` wrapper
    // (`tools/cc`, AGENTS.md §12), links them into one PIE image, and converts
    // it to an `rxe` blob (`rustos_itest_harness::elf2rxe`) carrying the
    // kernel's syscall CFI tag. On boot the test enables `CPACR_EL1.FPEN`,
    // stands up a stage-1 address space (identity-mapping the kernel + MMIO,
    // EL1), installs the EL1 vector table and a dispatch callback, then calls
    // the production capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's EL0 image and `eret` into it. The C program checks a
    // Time64 value across the §21 pre-1970/post-2038 boundaries, an ipc header,
    // and a sysinfo header, then issues `cap_query` + `clock_get`; the callback
    // services those (asserting the marshalled cap id, returning a 64-bit
    // sentinel) and asserts the `exit` code is 99 before the ARM semihosting
    // PASS finisher. Single CPU; 60-second run budget.
    QemuTest {
        package: "rustos-test-c-program-qemu-aarch64",
        binary: "rustos-test-c-program-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the x86_64
    // end-to-end C-program round-trip — the ring-3 analogue of the
    // riscv64/aarch64 verticals above, completing CC5. The build script builds
    // the Rust crt0 + `ros_sys_*` runtime shim (`tests/integration/cc5_program`)
    // as a PIE `staticlib`, compiles the genuinely C-language program
    // (`cc5_program/csrc/main.c`) with the audited, version-pinned, checksummed
    // `clang`/`ld.lld` wrapper (`tools/cc`, AGENTS.md §12), links them into one
    // PIE image, and converts it to an `rxe` blob (`rustos_itest_harness::elf2rxe`)
    // carrying the kernel's syscall CFI tag. Because the x86_64 ring-3
    // transition needs the GDT user selectors, the TSS, and `syscall`/
    // `IA32_LSTAR` entry installed, the test boots the production kernel pipeline
    // and, on `AuditEvent::BootCompleted`, enables `IA32_EFER.NXE`, builds a
    // fresh address space (low 32 MiB identity + higher-half kernel window),
    // switches CR3, installs a dispatch callback, then calls the production
    // capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to build
    // the program's ring-3 image (W^X: code RX, data RW-NX, rodata R-NX) and
    // `iretq` into it. The C program checks a Time64 value across the §21
    // pre-1970/post-2038 boundaries, an ipc header, and a sysinfo header, then
    // issues `cap_query` + `clock_get`; the callback services those (asserting
    // the marshalled cap id, returning a 64-bit sentinel) and asserts the `exit`
    // code is 99 before `qemu_exit::exit_success`. Single CPU; 60-second budget.
    QemuTest {
        package: "rustos-test-c-program-qemu-x86_64",
        binary: "rustos-test-c-program-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4 deliverable: boot the production kernel pipeline,
    // instantiate `rustos_drvhost::Host`, load a baked-in signed
    // mock `.rxe` image, exercise `load → snapshot → reload →
    // unload`, then flip `qemu_exit::exit_success`. Single CPU
    // suffices and the 60-second budget matches the other Stage 3a
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-drvhost-qemu",
        binary: "rustos-test-drvhost-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4 first-driver vertical: boot the production kernel
    // pipeline, then on `AuditEvent::BootCompleted` load the signed
    // PS/2 input driver (`drivers/input/ps2`) through
    // `rustos_drvhost::Host` and drive it through load -> use ->
    // unload -> reload. "Use" is interrupt-driven: it binds the
    // keyboard line (ISA IRQ-1 -> GSI 1) in the production
    // `rustos_kernel_irq::IrqTable`, enables the i8042 keyboard-
    // interrupt config bit, masks the legacy PIC, unmasks GSI 1 at the
    // IO-APIC, then injects a deterministic scancode via the
    // controller's `0xD2` ("write keyboard output buffer") command —
    // using the same `X86PortIo8` backend the driver reads through —
    // which asserts the real IRQ-1 line. After `sti` it waits on
    // `IrqTable::try_wait_step` for the IO-APIC -> LAPIC -> IDT ->
    // dispatcher -> `IrqTable::fire` round-trip to report
    // `WaitStep::Ready`, then drains and decodes the resulting press
    // then release into platform-neutral `InputEvent`s through the
    // driver's `poll`. Any deviation flips `qemu_exit::exit_failure`.
    // The default `q35` machine exposes the i8042 and a 24-pin
    // IO-APIC, so no extra QEMU device is needed. Single CPU suffices
    // and the 60-second budget matches the other Stage-3/4
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-ps2-qemu-x86-64",
        binary: "rustos-test-ps2-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4.D Item 2-tail.2 QEMU validation: boot the production
    // kernel pipeline, then drive a real hardware-interrupt round
    // trip on the legacy IRQ-0 GSI through the IO-APIC + PIT. The
    // test binary `rustos-test-irq-qemu-x86-64` installs an audit
    // sink that — on observing `AuditEvent::BootCompleted` — binds
    // the line in the published `IrqTable`, unmasks through the
    // production `IoApicController`, programs PIT channel 0 as a
    // one-shot, polls `IrqTable::try_wait_step` until
    // `WaitStep::Ready`, re-reads the IO-APIC redirection-entry
    // mask bit to verify the mask-before-wake invariant, and flips
    // `qemu_exit::exit_success`. Any deviation flips
    // `qemu_exit::exit_failure`. Single CPU suffices and a 60-second
    // budget matches the other Stage-3/4 boot-then-do-fixed-work
    // tests.
    QemuTest {
        package: "rustos-test-irq-qemu-x86-64",
        binary: "rustos-test-irq-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4.D Item 4: `rustos-test-virtio-blk-pci-x86-64` performs a
    // full real virtio-blk-pci round-trip — boot → `mechanism_one`
    // PCI walk → map the four virtio register windows → route MSI-X →
    // mint a `KernelVirtioHost` over a per-device DMA pool → load the
    // signed virtio-blk `.rxe` → read sector 0 (verify the planted
    // `byte[i] = i mod 256` pattern) → write+read-back sector 1
    // (verify) → `qemu_exit`. The earlier ~30% single-CPU MSI
    // completion hang was a deadlock between the completion ISR's
    // `IrqTable::fire` and a parked `try_wait_step`; it was eliminated
    // by making `fire`/`try_wait_step` lock-free (per-line `bound` /
    // `ready` atomics, no shared `IrqTable` lock). Stability re-verified
    // across 90 consecutive QEMU runs (60 TCG via this exact runner
    // path + 30 KVM) with zero hangs, so it is enrolled here. The
    // 2048-sector backing image gives the planted sector-0 pattern plus
    // headroom for the sector-1 write/read-back. A 60-second budget
    // matches the other Stage-3/4 boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-virtio-blk-pci-x86-64",
        binary: "rustos-test-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: Some(2048),
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 5 end-to-end FAT32 vertical: `rustos-test-fat32-virtio-blk-
    // pci-x86-64` reuses the exact virtio-blk-pci bring-up above, then
    // instead of a raw sector round-trip it mounts the planted FAT32
    // volume through the real FAT32 driver, verifies the planted file,
    // and creates+writes+reads-back a fresh file before `qemu_exit`.
    // The backing image is the shared `rustos-test-fat32-image` FAT32
    // volume (`FsDisk::Fat32`), not the sector-0 pattern, so its geometry
    // is the image's own size. Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-fat32-virtio-blk-pci-x86-64",
        binary: "rustos-test-fat32-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::Fat32,
        keyboard: None,
    },
    // Stage 5 end-to-end rustfs vertical: `rustos-test-rustfs-virtio-blk-
    // pci-x86-64` reuses the exact virtio-blk-pci bring-up above, then
    // instead of a raw sector round-trip it mounts the planted rustfs
    // volume through the real rustfs driver, verifies the planted file,
    // and creates+writes+reads-back a fresh file before `qemu_exit`.
    // The backing image is the shared `rustos-test-rustfs-image` rustfs
    // volume (`FsDisk::Rustfs`) — which the driver itself authored — not
    // the sector-0 pattern, so its geometry is the image's own size.
    // Single CPU and a 60-second budget match the FAT32 vertical and the
    // other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-rustfs-virtio-blk-pci-x86-64",
        binary: "rustos-test-rustfs-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::Rustfs,
        keyboard: None,
    },
    // Stage 4.D Item 4: `rustos-test-virtio-net-pci-x86-64` performs a
    // full real virtio-net-pci round-trip on the same shared bring-up
    // scaffolding as the virtio-blk vertical — boot → `mechanism_one`
    // PCI walk → map the four virtio register windows → route MSI-X →
    // mint a `KernelVirtioHost` over a per-device DMA pool → load the
    // signed virtio-net `.rxe` → drive `rustos-net-icmp` over the device:
    // ARP-resolve the QEMU user-mode (SLIRP) gateway `10.0.2.2` from guest
    // `10.0.2.15`, then send an ICMP echo and confirm the reply →
    // `qemu_exit`. A user-mode netdev (no host privileges) plus a frame
    // dump to `<binary>.pcap` lets a host inspect the exchange after the
    // run. The guest must initiate (SLIRP never pings the guest), which
    // the `rustos-net-icmp` `Client` does. Single CPU and a 60-second
    // budget match the other Stage-3/4 boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-virtio-net-pci-x86-64",
        binary: "rustos-test-virtio-net-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: true,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4.D Item 4: `rustos-test-kernel-arch-boot-riscv64` boots
    // the riscv64 `virt`-board pipeline (OpenSBI → S-mode entry →
    // FDT `/memory` parse → `RiscvArch` → `BootInfo` →
    // `kernel_core::kernel_main`) and asserts `AuditEvent::BootCompleted`
    // (`EventId(4004)`). The bin's audit sink writes the `SiFive` Test
    // PASS finisher on observing it. Single CPU suffices (the slice
    // brings up one hart) and a 60-second budget matches the x86_64
    // `kernel_arch_boot` bring-up test.
    QemuTest {
        package: "rustos-test-kernel-arch-boot-riscv64",
        binary: "rustos-test-kernel-arch-boot-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3c: `rustos-test-timer-preempt-qemu-riscv64` is the riscv64
    // half of the Stage-3 "timer interrupt drives the scheduler"
    // per-sub-stage deliverable. It boots the `virt` board, reads the
    // device-tree `timebase-frequency`, installs a `preempt`
    // scheduler-tick callback, arms the SBI timer at 100 Hz + enables
    // `sie.STIE`, and idles on `wfi` until the supervisor-timer trap path
    // has driven the callback 20 times — proving the timer repeatedly
    // delivers and re-arms — then writes the `SiFive` Test PASS finisher.
    // A revert to no-timer scheduling never reaches the count, so the run
    // times out and the harness reports the failure. Single CPU (the
    // slice brings up one hart) and a 60-second budget match the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-timer-preempt-qemu-riscv64",
        binary: "rustos-test-timer-preempt-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3c: `rustos-test-ipi-smp-qemu-riscv64` is the riscv64
    // multi-hart SMP deliverable. It boots the `virt` board with two
    // harts, derives the boot hart id at runtime (OpenSBI may boot on
    // either), starts the other hart through `smp::start_secondary` (the
    // SBI HSM `hart_start` call), waits for that hart to install its trap
    // vector and enable supervisor software interrupts, then sends it a
    // directed IPI through `RiscvArch::send_ipi` (the SBI IPI extension,
    // replacing the former no-op). The test passes once the secondary
    // hart's `sip.SSIP` trap path has run the IPI callback with the
    // secondary hart's id — proving both hart bring-up and IPI delivery.
    // A regression that fails to start the hart or deliver the IPI never
    // reaches the PASS finisher, so the run times out. Two CPUs (the
    // point of the test) and a 60-second budget.
    QemuTest {
        package: "rustos-test-ipi-smp-qemu-riscv64",
        binary: "rustos-test-ipi-smp-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 2,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // WIRING Stage W6 (`plans/WIRING.md` §3): the aarch64 multi-core SMP
    // deliverable — the EL1/GICv2 analogue of `ipi_smp_qemu_riscv64`. It
    // boots the `virt` board with two cores, starts core 1 through
    // `smp::start_secondary` (the PSCI `CPU_ON` call), waits for that core
    // to bring up its GICv2 interface and enable the IPI SGI, then sends
    // it a directed IPI through `Aarch64Arch::send_ipi` (a GICv2 SGI,
    // replacing the former single-CPU self-target best-effort send). The
    // test passes once the secondary core's IRQ path has run the IPI
    // callback with the secondary core's id — proving both core bring-up
    // and IPI delivery. A regression that fails to start the core or
    // deliver the IPI never reaches the PASS finisher, so the run times
    // out. Two CPUs (the point of the test) and a 60-second budget.
    QemuTest {
        package: "rustos-test-ipi-smp-qemu-aarch64",
        binary: "rustos-test-ipi-smp-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 2,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3c: `rustos-test-sched-drive-qemu-riscv64` is the riscv64
    // "arch primitives drive the live scheduler" deliverable — the wiring
    // that connects the `preempt` (timer + IPI) and `context` primitives
    // into the architecture-neutral `kernel/sched` `Scheduler`, rather
    // than the test-local counting callbacks the `timer_preempt` /
    // `ipi_smp` verticals use. It boots the `virt` board, performs a real
    // bidirectional `context::switch` round-trip (interrupts off), builds
    // a real `rustos-kernel-sched-mlfq::Scheduler` over `RiscvArch`,
    // installs the `preempt` timer callback and the IPI software-interrupt
    // callback so both drive `Scheduler::on_timer_tick`, arms the 100 Hz
    // SBI timer + IPI, spawns a batch of tasks, sends itself a directed
    // IPI, and drives the cooperative `step` loop until every task has
    // run. PASS once the supervisor-timer trap has driven the live
    // scheduler >= 20 times and the IPI software-interrupt path has driven
    // it at least once. A regression that fails to switch, dispatch,
    // tick, or deliver the IPI either trips a dedicated failure finisher
    // or never reaches PASS, so the run fails loudly. Single CPU (the
    // slice brings up one hart) and a 60-second budget match the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-sched-drive-qemu-riscv64",
        binary: "rustos-test-sched-drive-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // WIRING Stage W7 (`plans/WIRING.md` §3): the aarch64 "arch
    // primitives drive the live scheduler" deliverable — the EL1/GICv2
    // analogue of `sched_drive_qemu_riscv64`. It boots the `virt` board,
    // performs a real bidirectional `context::switch` round-trip
    // (interrupts off), builds a real `rustos-kernel-sched-mlfq::Scheduler`
    // over `Aarch64Arch`, installs the `preempt` generic-timer callback
    // and the GICv2 IPI (SGI) callback so both drive
    // `Scheduler::on_timer_tick`, brings up the EL1 vectors + GICv2, arms
    // the 100 Hz generic timer + IPI, spawns a batch of tasks, sends
    // itself a directed IPI, and drives the cooperative `step` loop until
    // every task has run. PASS once the generic-timer IRQ has driven the
    // live scheduler >= 20 times and the IPI SGI path has driven it at
    // least once. PI Stage P4 (`plans/PI.md`): the tick interval is sized
    // from the timer frequency *discovered* from the embedded `virt` DTB
    // (`kernel_arch::timer_frequency_hz`) and the GICv2 base is poisoned
    // then rediscovered (`gic::configure_from_fdt`) before `gic::init`, so
    // both the timer ticks and the IPI run over discovered values, not the
    // pre-discovery defaults. A regression that fails to switch, dispatch,
    // tick, or deliver the IPI either trips a dedicated failure finisher or
    // never reaches PASS, so the run fails loudly. Single CPU (the slice
    // brings up one core) and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-sched-drive-qemu-aarch64",
        binary: "rustos-test-sched-drive-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // SPAWN Stage SP1 (`plans/SPAWN.md` §1): the `kernel/core` kthread
    // runtime proven on real silicon — two kernel-thread tasks ping-pong
    // through the *real* `rustos_arch_api::ContextSwitch::switch` under the
    // live scheduler, making that primitive a production scheduling path
    // for the first time (until now it was exercised only by the W7
    // `sched_drive` round-trip). It boots the `virt` board, reads the
    // GICv2 base + timer rate from the embedded `virt` DTB and brings up
    // the EL1 vectors + GICv2 (interrupts stay masked — dispatch is the
    // cooperative `step` loop, so the kthread switches are the only
    // mechanism under test), builds a real `rustos-kernel-sched-mlfq`
    // `Scheduler` over `Aarch64Arch`, spawns two kthreads via
    // `kernel_core::spawn_kthread` whose bodies `yield_now` back and forth,
    // and drains the `step` loop. PASS once both kthreads have run their
    // full ping-pong count and exited; a switch that never resumed its
    // task stalls the drain and the harness reports a timeout (fail-loud,
    // `AGENTS.md` §7). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-kthread-switch-qemu-aarch64",
        binary: "rustos-test-kthread-switch-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // SPAWN Stage SP1 (`plans/SPAWN.md` §1): the riscv64 sibling of the
    // aarch64 kthread-switch vertical above — the same "two kthreads
    // ping-pong through the *real* `rustos_arch_api::ContextSwitch::switch`
    // under the live scheduler" proof, now on the riscv64 `virt` board, so
    // the `kernel/core` kthread runtime is a production scheduling path on
    // riscv64 too. It boots `virt`, reads the generic-timer rate from the
    // firmware DTB (the verbatim `a1` pointer), builds a real
    // `rustos-kernel-sched-eevdf` `Scheduler` over `RiscvArch`, spawns two
    // kthreads via `kernel_core::spawn_kthread` whose bodies `yield_now`
    // back and forth (interrupts stay masked — dispatch is the cooperative
    // `step` loop), and drains the loop. PASS once both kthreads have run
    // their full ping-pong count and exited; a switch that never resumed
    // its task stalls the drain and the harness reports a timeout
    // (fail-loud, `AGENTS.md` §7). Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-kthread-switch-qemu-riscv64",
        binary: "rustos-test-kthread-switch-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // SPAWN Stage SP1 (`plans/SPAWN.md` §1): the x86_64 sibling of the
    // kthread-switch vertical — the same "two kthreads ping-pong through
    // the *real* `rustos_arch_api::ContextSwitch::switch` under the live
    // scheduler" proof on the multiboot-loaded x86_64 kernel, so the
    // `kernel/core` kthread runtime is a production scheduling path on
    // x86_64 too. On the boot CPU it installs the per-CPU GDT/IDT, builds a
    // real `rustos-kernel-sched-eevdf` `Scheduler` over the production
    // `X86_64Arch` handle (no AP bring-up, no LAPIC timer — interrupts stay
    // masked, so the spawn self-IPI is latched and never delivered), spawns
    // two kthreads via `kernel_core::spawn_kthread` whose bodies `yield_now`
    // back and forth, and drains the cooperative `step` loop. PASS once both
    // kthreads have run their full ping-pong count and exited; a switch that
    // never resumed its task stalls the drain and the harness reports a
    // timeout (fail-loud, `AGENTS.md` §7). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-kthread-switch-qemu-x86-64",
        binary: "rustos-test-kthread-switch-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // WIRING Stage W6 (`plans/WIRING.md` §3): the cross-CPU TLB-shootdown
    // HAL slice (`rustos_arch_api::CrossCpuTlbShootdown`) proven on real
    // emulated cores, one vertical per bare-metal port. riscv64: the boot
    // hart starts a second hart, then `RiscvArch::shootdown_page` runs the
    // local `sfence.vma` + the SBI RFENCE `remote_sfence_vma` firmware call
    // to the live hart, and the test asserts the firmware reports the
    // remote fence reached it. Two CPUs (the point of the test) and a
    // 60-second budget match the other multi-hart riscv64 tests.
    QemuTest {
        package: "rustos-test-cross-cpu-tlb-shootdown-qemu-riscv64",
        binary: "rustos-test-cross-cpu-tlb-shootdown-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 2,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // WIRING Stage W6: the aarch64 cross-CPU TLB-shootdown vertical. The
    // boot core starts a second core via PSCI `CPU_ON`, then
    // `Aarch64Arch::shootdown_page` issues the inner-shareable *broadcast*
    // `tlbi vaae1is` + `dsb ish`/`isb` — the hardware propagates it to
    // every PE in the domain, so no IPI or software acknowledge is needed.
    // Reaching PASS proves the broadcast executes on a real two-core
    // machine without faulting. Two CPUs and a 60-second budget.
    QemuTest {
        package: "rustos-test-cross-cpu-tlb-shootdown-qemu-aarch64",
        binary: "rustos-test-cross-cpu-tlb-shootdown-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 2,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // WIRING Stage W6: the x86_64 cross-CPU TLB-shootdown vertical — the
    // port whose cross-CPU invalidation is entirely hand-written software
    // (no broadcast `invlpg`). The BSP brings up an application processor
    // via INIT-SIPI-SIPI; both install the shootdown ISR; the BSP drives
    // `X86_64Arch::shootdown_page`, which IPIs the AP and spins on the
    // acknowledge counter, returning only once the AP's ISR has `invlpg`'d
    // and acknowledged. Reaching PASS proves the IPI + invalidation + ack
    // round-trip ran on a second real core. Two CPUs and a 60-second
    // budget.
    QemuTest {
        package: "rustos-test-cross-cpu-tlb-shootdown-qemu-x86-64",
        binary: "rustos-test-cross-cpu-tlb-shootdown-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 2,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3c: `rustos-test-memory-isolation-qemu-riscv64` is the riscv64
    // half of the Stage-3 "memory-isolation test passes" per-sub-stage
    // deliverable — the riscv64 analogue of `rustos-test-memory-isolation`
    // (x86_64). It boots the `virt` board, builds a victim and an attacker
    // Sv39 `paging::AddressSpace` (each identity-maps the low 4 GiB) that
    // disagree on a single 64 GiB virtual address, installs a `fault`
    // handler, switches `satp` to the attacker space, and reads that
    // address: the MMU raises a load page fault, the handler confirms the
    // cause / faulting address / victim-intact invariants, and writes the
    // `SiFive` Test PASS finisher. A regression that fails to isolate the
    // address never faults and trips the failure finisher instead. Single
    // CPU (the slice brings up one hart) and a 60-second budget match the
    // other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-memory-isolation-qemu-riscv64",
        binary: "rustos-test-memory-isolation-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4.D Item 4: `rustos-test-virtio-blk-mmio-riscv64` is the
    // riscv64 `virt`-board MMIO analogue of the x86_64 virtio-blk-pci
    // vertical — boot → build the virtio-MMIO bus from the device tree →
    // provision an `MmioTransport` through the capability-gated
    // `KernelMmioMapper` → arm the device's PLIC source + S-mode trap
    // path → mint a `KernelVirtioHost` over a carved per-device DMA pool
    // → load the signed virtio-blk `.rxe` → read sector 0 (verify the
    // planted `byte[i] = i mod 256` pattern) → write+read-back sector 1 →
    // `SiFive` Test PASS. The device-tail round-trip is the same shared
    // code the x86_64 vertical runs. The 2048-sector backing image gives
    // the planted sector-0 pattern plus headroom; single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-virtio-blk-mmio-riscv64",
        binary: "rustos-test-virtio-blk-mmio-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: Some(2048),
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4.D Item 4: `rustos-test-virtio-net-mmio-riscv64` is the
    // riscv64 `virt`-board MMIO analogue of the x86_64 virtio-net-pci
    // vertical — same bring-up as the blk MMIO vertical, then drive
    // `rustos-net-icmp` over the device: ARP-resolve the QEMU user-mode
    // (SLIRP) gateway `10.0.2.2` from guest `10.0.2.15`, then send an
    // ICMP echo and confirm the reply → `SiFive` Test PASS. The
    // device-tail ping is the same shared code the x86_64 vertical runs.
    // A user-mode netdev (no host privileges) plus a frame dump to
    // `<binary>.pcap` lets a host inspect the exchange. Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-virtio-net-mmio-riscv64",
        binary: "rustos-test-virtio-net-mmio-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: true,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4 first-driver vertical (display class):
    // `rustos-test-framebuffer-display-qemu-riscv64` boots the riscv64
    // `virt`-board pipeline, programs QEMU's `ramfb` over the `fw_cfg`
    // MMIO DMA interface so a static guest-RAM surface becomes a real
    // scan-out framebuffer, publishes the geometry as a
    // `FramebufferConfig` boot hand-off, then loads the signed
    // framebuffer display `.rxe` through `rustos_drvhost::Host` and
    // drives it through load -> use -> unload -> reload. "Use" maps the
    // surface through the capability-gated `KernelMmioMapper` and
    // `present`s a frame; a second independently-mapped window reads the
    // pixels back to confirm they reached the scan-out memory. Any
    // deviation flips the `SiFive` Test failure finisher. Single CPU and
    // a 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-framebuffer-display-qemu-riscv64",
        binary: "rustos-test-framebuffer-display-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 4 first-driver vertical (display class, x86_64 sibling of the
    // framebuffer vertical): `rustos-test-vesa-qemu-x86-64` boots the
    // production kernel pipeline, programs QEMU's `ramfb` over the
    // `fw_cfg` IOport DMA interface so a static guest-RAM surface becomes
    // a real scan-out framebuffer, publishes a bootloader-captured VBE
    // `ModeInfoBlock` describing it as the boot hand-off, then loads the
    // signed vesa display `.rxe` through `rustos_drvhost::Host` and drives
    // it through load -> use -> unload -> reload. "Use" decodes the block
    // with `VesaFramebuffer::open`, maps the surface through the
    // capability-gated `KernelMmioMapper`, and `present`s a frame; a
    // second independently-mapped window reads the pixels back to confirm
    // they reached the scan-out memory. Any deviation flips
    // `qemu_exit::exit_failure`. Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-vesa-qemu-x86-64",
        binary: "rustos-test-vesa-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // PI Stage P6c-2 (`plans/PI.md`): `rustos-test-kernel-arch-boot-aarch64`
    // boots the *production* aarch64 `rustos-kernel` pipeline
    // (`boot_aarch64::boot`) on the `virt` board all the way to
    // `AuditEvent::BootCompleted` — the aarch64 analogue of the x86_64
    // `kernel-arch-boot` and the riscv64 `kernel-arch-boot-riscv64`
    // verticals. It enables the stage-1 identity MMU + EL1 vectors,
    // discovers the board from the embedded `virt` device tree (QEMU's
    // aarch64 `-kernel <ELF>` path passes no `x0` DTB pointer), builds the
    // `BootMemoryMap`, installs the discovered-UART console + `svc`
    // dispatch callback, and hands a validated `BootInfo` to
    // `kernel_core::kernel_main`; the audit sink reports PASS through the
    // ARM semihosting finisher on `EventId(4004)`. Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-kernel-arch-boot-aarch64",
        binary: "rustos-test-kernel-arch-boot-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // PI Stage P6c-3 (`plans/PI.md`): `rustos-test-spawn-init-qemu-aarch64`
    // boots the *production* aarch64 `rustos-kernel` pipeline
    // (`boot_aarch64::boot`) on the `virt` board, then drops into PID 1
    // (`init`) in EL0 through the `InitSpawn` seam `boot_aarch64` installs
    // into the `BootInfo` hand-off. After `kernel_core::kernel_main` emits
    // `AuditEvent::BootCompleted` it builds the embedded `init` (`Run`) EL0
    // image through the capability-checked, audited `spawn_and_enter`
    // (emitting `ProcessSpawned`, `EventId(4030)`) and `eret`s into it;
    // `init` returns and the `rustos-rt` runtime routes the return through
    // the audited `exit` syscall, whose `svc` traps back through the EL1
    // vector to the production dispatch callback (emitting `SyscallInvoked`,
    // `EventId(5000)`). The audit sink reports PASS through the ARM
    // semihosting finisher once it has seen `ProcessSpawned` then
    // `SyscallInvoked` — proving PID 1 reached user mode and trapped back.
    // Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-spawn-init-qemu-aarch64",
        binary: "rustos-test-spawn-init-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // SPAWN Stage SP2c (`plans/SPAWN.md` §1): the aarch64 EL0↔EL0 timeshare
    // vertical — the first proof that two **user** (EL0) tasks timeshare one
    // CPU under the live scheduler, on the `virt` board. It reads the GICv2
    // base + timer rate from the embedded `virt` DTB (P3/P4), brings up the
    // EL1 vectors + GICv2 (interrupts stay masked — dispatch is the cooperative
    // `step` loop), and builds **two** hardware-isolated EL0 address spaces from
    // the pure-Rust `rustos-test-el0-yielder` fixture (built PIE + converted to
    // `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It admits each as a resumable user kthread via
    // `spawn_user_kthread` (its `pre_resume` hook reactivates that task's
    // page-table root, §4) and drains the `step` loop; the dispatch callback
    // maps each task's `yield`/`exit` `svc` to `reschedule_current`, suspending
    // the running task back to the dispatcher exactly as the production callback
    // does. PASS once both tasks yielded their full count and exited; a switch
    // that never resumes stalls the drain and the harness times out (fail-loud,
    // `AGENTS.md` §7). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-spawn-el0-timeshare-qemu-aarch64",
        binary: "rustos-test-spawn-el0-timeshare-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // PI Stage P2 (`plans/PI.md`): `rustos-test-uart-console-qemu-aarch64`
    // is the runtime proof of the board-discovered console. It boots the
    // `virt` board through the arch crate's EL1 trampoline, poisons the
    // console base with a deliberately-wrong value, parses the canonical
    // QEMU `virt` device tree embedded at build time (QEMU's aarch64
    // `-kernel <ELF>` path passes no DTB pointer in `x0`), and calls
    // `console::configure_from_fdt`. It then asserts the base moved off the
    // poison value to the PL011 the tree advertised and logs two lines over
    // the *discovered* console before the ARM semihosting PASS finisher —
    // proving the console MMIO base is now sourced from the firmware device
    // tree, not a compile-time constant, and that writes reach it. (The
    // Pi's specific console base is host-unit-tested against the
    // `raspi_like_arm` fixture and is an on-metal acceptance item: QEMU's
    // `raspi*` models do not model the GPU-firmware DTB hand-off.) Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "rustos-test-uart-console-qemu-aarch64",
        binary: "rustos-test-uart-console-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage W11 (`plans/WIRING.md` §3):
    // `rustos-test-virtio-blk-mmio-aarch64` is the aarch64 `virt`-board
    // MMIO analogue of the riscv64 virtio-blk-mmio vertical — boot the
    // arch crate's EL1 trampoline → build the virtio-MMIO bus from the
    // device tree → provision an `MmioTransport` through the capability-
    // gated `KernelMmioMapper` → arm the device's GICv2 SPI + EL1 IRQ
    // path → mint a `KernelVirtioHost` over a static per-device DMA pool →
    // load the signed virtio-blk `.rxe` → read sector 0 (verify the
    // planted `byte[i] = i mod 256` pattern) → write+read-back sector 1 →
    // ARM semihosting PASS. The device-tail round-trip is the same shared
    // code the riscv64 / x86_64 verticals run. The 2048-sector backing
    // image gives the planted sector-0 pattern plus headroom; single CPU
    // and a 60-second budget match the other boot-then-do-fixed-work
    // tests.
    QemuTest {
        package: "rustos-test-virtio-blk-mmio-aarch64",
        binary: "rustos-test-virtio-blk-mmio-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: Some(2048),
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage W11 (`plans/WIRING.md` §3):
    // `rustos-test-virtio-net-mmio-aarch64` is the aarch64 `virt`-board
    // MMIO analogue of the riscv64 virtio-net-mmio vertical — same
    // bring-up as the blk MMIO vertical, then drive `rustos-net-icmp`
    // over the device: ARP-resolve the QEMU user-mode (SLIRP) gateway
    // `10.0.2.2` from guest `10.0.2.15`, then send an ICMP echo and
    // confirm the reply → ARM semihosting PASS. The device-tail ping is
    // the same shared code the riscv64 / x86_64 verticals run. A
    // user-mode netdev (no host privileges) plus a frame dump to
    // `<binary>.pcap` lets a host inspect the exchange. Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-virtio-net-mmio-aarch64",
        binary: "rustos-test-virtio-net-mmio-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: true,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage W11-B (`plans/WIRING.md` §3): the aarch64 display vertical —
    // the EL1/GICv2 + ramfb analogue of the riscv64 framebuffer-display
    // vertical. `rustos-test-framebuffer-display-qemu-aarch64` brings the
    // `virt` board up to EL1 (FP enable + 2 GiB identity MMU + vectors,
    // shared from `virtio_qemu_support`), programs QEMU's `ramfb` over the
    // shared `fw_cfg` MMIO DMA interface so a static guest-RAM surface
    // becomes a real scan-out framebuffer, assembles the geometry as a
    // `FramebufferConfig`, then loads the signed framebuffer display
    // `.rxe` through `rustos_drvhost::Host` and drives it through
    // load -> use -> unload -> reload. "Use" maps the surface through the
    // capability-gated `KernelMmioMapper` and `present`s a frame; a second
    // independently-mapped window reads the pixels back to confirm they
    // reached the scan-out memory. Any deviation flips the ARM semihosting
    // failure finisher. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-framebuffer-display-qemu-aarch64",
        binary: "rustos-test-framebuffer-display-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3b: `rustos-test-timer-preempt-qemu-aarch64` is the aarch64
    // half of the Stage-3 "timer interrupt drives the scheduler"
    // per-sub-stage deliverable. It installs the EL1 vectors, brings up
    // the GICv2, arms the EL1 physical generic timer at 100 Hz, unmasks
    // IRQs, and idles on `wfi` until the generic-timer IRQ path has
    // driven the `preempt` callback 20 times — proving the timer
    // repeatedly delivers and re-arms — then reports PASS via semihosting.
    // Single CPU and a 60-second budget.
    QemuTest {
        package: "rustos-test-timer-preempt-qemu-aarch64",
        binary: "rustos-test-timer-preempt-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage 3b: `rustos-test-memory-isolation-qemu-aarch64` is the
    // aarch64 half of the Stage-3 "memory-isolation test passes"
    // per-sub-stage deliverable — the analogue of the x86_64 and riscv64
    // verticals. It builds a victim and an attacker stage-1
    // `paging::AddressSpace` (each identity-maps the low 2 GiB) that
    // disagree on a single 64 GiB page, installs the EL1 vectors and a
    // `fault` handler, switches `TTBR0_EL1` to the attacker (enabling the
    // MMU), and reads that page: the MMU raises a data abort, the handler
    // confirms the cause / faulting address, and reports PASS via
    // semihosting. A regression that fails to isolate the page reads it
    // without faulting and reports FAILURE explicitly. Single CPU and a
    // 60-second budget.
    QemuTest {
        package: "rustos-test-memory-isolation-qemu-aarch64",
        binary: "rustos-test-memory-isolation-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // WIRING Stage W3-B (`plans/WIRING.md` §3): the aarch64 device-IRQ
    // vertical — the EL1/GICv2-SPI analogue of `rustos-test-irq-qemu-x86-64`.
    // `rustos-test-irq-qemu-aarch64` installs the EL1 vectors, brings up the
    // GICv2, builds a kernel-neutral `rustos_kernel_irq::IrqTable`, binds the
    // PL031 RTC's shared-peripheral interrupt (INTID 34), routes that SPI to
    // CPU 0 through the new `gic::route_spi` (`GICD_ITARGETSR`), installs a
    // set-once device-IRQ dispatcher (`exceptions::set_device_irq_dispatch`)
    // that forwards the line to `IrqTable::fire` over a `GicController`
    // bridge, arms the RTC match, and unmasks IRQs. When the RTC fires, the
    // GIC delivers the SPI to EL1, the dispatcher masks the line and sets the
    // wait flag, and the main loop observes `WaitStep::Ready`; it then
    // re-reads the GIC enable bit and asserts the line is masked (the
    // mask-before-wake invariant, `docs/src/security/irq.md`) before the ARM
    // semihosting PASS finisher. A regression that fails to route, deliver,
    // or mask never reaches PASS, so the run times out. Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-irq-qemu-aarch64",
        binary: "rustos-test-irq-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
    },
    // Stage W11-B (`plans/WIRING.md` §3): the aarch64 input vertical —
    // the `virt`-board virtio-input analogue of the x86_64 PS/2 vertical,
    // completing the `input` row of the §1 QEMU matrix for aarch64.
    // `rustos-test-input-virtio-mmio-qemu-aarch64` brings the `virt` board
    // up to EL1 (FP enable + 2 GiB identity MMU + GICv2/EL1 IRQ path,
    // shared from `virtio_qemu_support`), builds the virtio-MMIO bus from
    // the embedded device tree, provisions an `MmioTransport` through the
    // capability-gated `KernelMmioMapper`, arms the device's GICv2 SPI,
    // mints a `KernelVirtioHost`, loads the signed virtio-input `.rxe`
    // through `rustos_drvhost::Host`, and drives it through
    // load -> use -> unload -> reload. "Use" is a real injected key: once
    // the guest logs the event-queue-armed readiness marker, the runner
    // sends a key through the QEMU monitor (`sendkey`), the eventq IRQ
    // fires, and the driver decodes the press then (after reload) the
    // release. The runner attaches the `virtio-keyboard-device` and drives
    // the injection; the guest never fabricates the event. Single CPU and
    // a 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-input-virtio-mmio-qemu-aarch64",
        binary: "rustos-test-input-virtio-mmio-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: Some(("virtio-qemu: virtio-input eventq armed", "a")),
    },
    // WIRING (`plans/WIRING.md` §1/§3): the riscv64 input vertical —
    // the `virt`-board virtio-input MMIO analogue of the aarch64 input
    // vertical, completing the `input` row of the §1 QEMU matrix for
    // riscv64. `rustos-test-input-virtio-mmio-qemu-riscv64` boots the
    // `virt`-board pipeline, builds the virtio-MMIO bus from the device
    // tree, provisions an `MmioTransport` through the capability-gated
    // `KernelMmioMapper`, arms the device's PLIC source + S-mode trap
    // path, mints a `KernelVirtioHost`, loads the signed virtio-input
    // `.rxe` through `rustos_drvhost::Host`, and drives it through
    // load -> use -> unload -> reload. "Use" is a real injected key: once
    // the guest logs the event-queue-armed readiness marker, the runner
    // sends a key through the QEMU monitor (`sendkey`), the eventq IRQ
    // fires, and the driver decodes the press then (after reload) the
    // release. The runner attaches the `virtio-keyboard-device` and drives
    // the injection; the guest never fabricates the event. The driver and
    // the shared `virtio_input_keypress` tail are the same code the
    // aarch64 vertical runs (`AGENTS.md` §2.2). Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-input-virtio-mmio-qemu-riscv64",
        binary: "rustos-test-input-virtio-mmio-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: Some(("virtio-qemu: virtio-input eventq armed", "a")),
    },
];

/// Rust target triple for the riscv64 enrolments; selects the
/// `Spec::for_riscv64_kernel` constructor in [`run_one`].
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

/// Rust target triple for the aarch64 enrolments; selects the
/// `Spec::for_aarch64_kernel` constructor in [`run_one`].
const AARCH64_TARGET: &str = "aarch64-unknown-none";

/// Build every enrolled QEMU test once.
///
/// Call this before the (possibly repeated) [`run_once`] passes so a soak
/// re-runs the binaries rather than rebuilding them each pass (`AGENTS.md`
/// §7's no-flaky-tests rule: the value of repetition is in the *runs*).
pub fn build_all(ctx: &Context) -> Result<(), String> {
    eprintln!("xtask: [test --qemu] {} test(s) enrolled", TESTS.len());
    // Group the enrolled packages by target triple and build each triple in a
    // single `cargo build`. One invocation per triple (rather than one per
    // enrolment) lets cargo compile that triple's packages concurrently and
    // share a single build-lock acquisition, instead of serialising behind the
    // lock once per test. The QEMU *runs* themselves stay one-at-a-time — a
    // hard wall-clock deadline plus tick-counting guests make co-scheduled
    // VMs flaky under TCG (§7); see `commands::parallel`.
    for target in build_targets() {
        let packages: Vec<&str> = TESTS
            .iter()
            .filter(|t| t.target == target)
            .map(|t| t.package)
            .collect();
        let mut cmd = ctx.cargo();
        cmd.args(["build", "--locked", "--target", target]);
        for pkg in &packages {
            cmd.args(["-p", pkg]);
        }
        let label = format!("test --qemu (build {target}: {} pkg)", packages.len());
        ctx.run(&label, cmd)?;
    }
    Ok(())
}

/// The distinct target triples across the enrolled tests, in first-seen
/// order, so each triple is built exactly once.
fn build_targets() -> Vec<&'static str> {
    let mut targets: Vec<&'static str> = Vec::new();
    for t in TESTS {
        if !targets.contains(&t.target) {
            targets.push(t.target);
        }
    }
    targets
}

/// Execute every enrolled QEMU test once. Returns the first failure.
///
/// The caller ([`super::run_test`]) owns the repeat loop so a duration
/// budget covers the whole matrix as a unit; this runs exactly one pass and
/// never retries on failure (`AGENTS.md` §7).
pub fn run_once(ctx: &Context) -> Result<(), String> {
    for t in TESTS {
        run_one(ctx, t)?;
    }
    Ok(())
}

fn run_one(ctx: &Context, t: &QemuTest) -> Result<(), String> {
    let kernel: PathBuf = ctx.target_dir().join(t.target).join("debug").join(t.binary);
    // Select the per-arch QEMU `Spec`: the riscv64 enrolments boot the
    // `virt` board through OpenSBI; everything else uses the x86_64
    // `isa-debug-exit` convention.
    let base = if t.target == RISCV64_TARGET {
        Spec::for_riscv64_kernel(&kernel)
    } else if t.target == AARCH64_TARGET {
        Spec::for_aarch64_kernel(&kernel)
    } else {
        Spec::for_x86_64_kernel(&kernel)
    };
    let timeout = effective_timeout(t.timeout, super::in_github_actions());
    let mut spec = base.with_cpus(t.cpus).with_timeout(timeout);

    // Attach a planted raw backing image for storage tests. Sector 0
    // carries the deterministic `byte[i] = i mod 256` pattern the
    // kernel-side test reads back and verifies; every other sector
    // reads as zero, so the test's write+read-back of sector 1 cannot
    // pass on stale data.
    if let Some(sectors) = t.disk_sectors {
        let image = kernel.with_extension("blk.img");
        let sector0: Vec<u8> = (0..rustos_qemu::disk::SECTOR_BYTES)
            .map(|i| u8::try_from(i % 256).unwrap_or(0))
            .collect();
        rustos_qemu::disk::plant_raw_disk(&image, sectors, &[(0, &sector0)])
            .map_err(|e| format!("test --qemu ({}): plant backing disk: {e}", t.package))?;
        spec = spec.with_virtio_blk(&image);
    }

    // Attach the shared filesystem volume as the backing image, when the
    // enrolment names one. The bytes come from a single-source-of-truth
    // image fixture the kernel-side tail also names, so the planted
    // on-disk layout and the guest's expectations cannot drift
    // (`AGENTS.md` §2.2): the FAT32 fixture is hand-built; the rustfs
    // fixture is authored by the real rustfs driver itself (format +
    // plant). Only the non-zero sectors are planted; the planter
    // zero-fills the rest, matching a freshly-formatted volume.
    let fs_image: Option<(&str, Vec<u8>, u64)> = match t.fs_disk {
        FsDisk::None => None,
        FsDisk::Fat32 => Some((
            "fat32.img",
            rustos_test_fat32_image::build_image(),
            rustos_test_fat32_image::TOTAL_SECTORS,
        )),
        FsDisk::Rustfs => Some((
            "rustfs.img",
            rustos_test_rustfs_image::build_image()
                .map_err(|e| format!("test --qemu ({}): build rustfs image: {e:?}", t.package))?,
            rustos_test_rustfs_image::TOTAL_SECTORS,
        )),
    };
    if let Some((extension, bytes, total_sectors)) = fs_image {
        let image = kernel.with_extension(extension);
        let sector_bytes = rustos_qemu::disk::SECTOR_BYTES;
        let planted: Vec<(u64, &[u8])> = bytes
            .chunks(sector_bytes)
            .enumerate()
            .filter(|(_, chunk)| chunk.iter().any(|&b| b != 0))
            .map(|(lba, chunk)| (lba as u64, chunk))
            .collect();
        rustos_qemu::disk::plant_raw_disk(&image, total_sectors, &planted)
            .map_err(|e| format!("test --qemu ({}): plant filesystem disk: {e}", t.package))?;
        spec = spec.with_virtio_blk(&image);
    }

    // Attach a QEMU user-mode (SLIRP) virtio-net interface for networking
    // tests, dumping every frame to a `<binary>.pcap` capture beside the
    // kernel image so a failing run leaves the on-wire exchange to inspect.
    if t.virtio_net {
        let pcap = kernel.with_extension("pcap");
        spec = spec.with_virtio_net_pcap(&pcap);
    }

    // Attach a QEMU `ramfb` display device for the framebuffer vertical.
    if t.ramfb {
        spec = spec.with_ramfb();
    }

    // Attach a `virtio-keyboard-device` for the input vertical and let the
    // runner inject the key once the guest signals readiness on serial.
    if let Some((marker, key)) = t.keyboard {
        spec = spec.with_virtio_keyboard(marker, key);
    }

    eprintln!(
        "xtask: [test --qemu (run {})] kernel={} cpus={} timeout={:?}",
        t.package,
        kernel.display(),
        t.cpus,
        timeout
    );

    match Runner::run(&spec).map_err(|e| format!("test --qemu ({}): {e}", t.package))? {
        Outcome::Pass => Ok(()),
        Outcome::Fail { status, serial } => Err(format!(
            "test --qemu ({}) FAILED (qemu status {status})\n--- serial ---\n{serial}\n--- end ---",
            t.package
        )),
        Outcome::Timeout { budget, serial } => Err(format!(
            "test --qemu ({}) TIMEOUT after {budget:?} (no retries per AGENTS.md §7)\n--- serial ---\n{serial}\n--- end ---",
            t.package
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{build_targets, effective_timeout, DEVELOPER_TIMEOUT_CAP, TESTS};
    use std::time::Duration;

    #[test]
    fn build_targets_are_distinct_and_cover_every_enrolment() {
        let targets = build_targets();
        // No triple appears twice — each is built in exactly one invocation.
        for (i, a) in targets.iter().enumerate() {
            for b in &targets[i + 1..] {
                assert_ne!(a, b, "duplicate build target {a}");
            }
        }
        // Every enrolled test's triple is covered by the grouped build.
        for t in TESTS {
            assert!(
                targets.contains(&t.target),
                "build_targets missing {}",
                t.target
            );
        }
    }

    #[test]
    fn developer_machine_clamps_long_budgets_to_the_cap() {
        for secs in [60, 120] {
            assert_eq!(
                effective_timeout(Duration::from_secs(secs), false),
                DEVELOPER_TIMEOUT_CAP,
            );
        }
    }

    #[test]
    fn developer_machine_leaves_short_budgets_untouched() {
        let short = Duration::from_secs(10);
        assert_eq!(effective_timeout(short, false), short);
        assert_eq!(
            effective_timeout(DEVELOPER_TIMEOUT_CAP, false),
            DEVELOPER_TIMEOUT_CAP,
        );
    }

    #[test]
    fn ci_runner_keeps_the_full_enrolled_budget() {
        let full = Duration::from_secs(120);
        assert_eq!(effective_timeout(full, true), full);
    }
}
