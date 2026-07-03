//! QEMU integration-test driver invoked by `cargo xtask test --qemu`.
//!
//! the charter mandates that the QEMU tests share the same orchestrator
//! as host-side tests and that each QEMU run has a *strict* per-test
//! timeout with **no retries**. This module enforces both: it builds the
//! enrolled kernels per target triple, then drives each one through
//! [`rustos_qemu::Runner::run`], failing the whole `xtask test` invocation
//! if any guest fails or times out.
//!
//! The guests run **concurrently** through the shared weighted-concurrency
//! runner ([`super::parallel`]): each enrolment is independent (its own
//! per-binary backing images, a `-serial stdio` console, and a unique unix
//! monitor socket), so the only resource they contend for is host CPU. The
//! runner weights each guest by its emulated-CPU count against a budget of
//! the host's logical CPUs, so concurrent guest vCPUs never oversubscribe the
//! host and no guest is starved past its wall-clock deadline ('s
//! no-flaky-tests / no-retry rules hold). See [`run_once`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustos_qemu::{Outcome, Runner, Spec};

use super::parallel::{self, Job};
use crate::Context;

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
    /// Ordered serial-input script: for each `(marker, line)` step, pipe
    /// QEMU's stdin and write `line` to the guest's serial input once it
    /// prints `marker` on the serial console past the previous step's
    /// match. The run fails if the guest exits before every step was
    /// sent, so an unreached prompt is a test failure. Used by the
    /// aarch64 interactive-session vertical to hold a deterministic
    /// multi-exchange dialogue with the blocked login.
    serial: &'static [(&'static str, &'static str)],
}

/// Which filesystem volume (if any) the host harness plants on the
/// test's virtio-blk backing image. Each variant names a shared
/// single-source-of-truth image fixture.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FsDisk {
    /// No filesystem volume (the test uses `disk_sectors` or no disk).
    None,
    /// The shared [`rustos_test_fat32_image`] FAT32 volume.
    Fat32,
    /// The shared [`rustos_test_rustfs_image`] rustfs volume.
    Rustfs,
    /// The shared [`rustos_test_rustfs_image`] users-root volume:
    /// the standard filesystem tree with `/System/Security/Users` planted
    /// (`plans/PI.md` P11).
    UsersRoot,
    /// The shared [`rustos_test_encrypted_root_image`] whole-disk image: an
    /// MBR, a FAT boot partition carrying the `root.unlock` descriptor, and
    /// a passphrase-derived encrypted `RustFS` root carrying
    /// `/System/Security/Users` — the root-mount->login vertical's backing
    /// (`plans/PI.md` P11 Chunk B-2).
    EncryptedRootDisk,
    /// The shared [`rustos_test_autoload_root_image`] whole-disk image: the
    /// [`Self::EncryptedRootDisk`] layout whose **read-only `/System` volume**
    /// additionally carries a kernel-signed virtio-input keyboard driver bundle
    /// in its `Drivers/` store — the pre-unlock driver-loading-by-discovery
    /// autoload vertical's backing (`plans/PI.md` design B / B2).
    AutoloadRootDisk,
}

/// `true` if `line` is exactly `value` followed by a single `\n`.
///
/// Used by the compile-time checks that keep the root-unlock-admission
/// vertical's serial script in lockstep with the shared
/// [`rustos_test_encrypted_root_image`] fixture: the serial table needs
/// `&'static str` literals, so each typed line is verified against the
/// fixture's own constant at build time (single source
/// of truth; drift fails the build rather than silently mistyping at the
/// prompt).
const fn is_line_of(line: &[u8], value: &[u8]) -> bool {
    if line.len() != value.len() + 1 {
        return false;
    }
    let mut i = 0;
    while i < value.len() {
        if line[i] != value[i] {
            return false;
        }
        i += 1;
    }
    line[value.len()] == b'\n'
}

/// The passphrase line the admission vertical types at `Root passphrase: `.
const UNLOCK_PASSPHRASE_LINE: &str = "unlock-vertical correct horse battery staple\n";

/// Serial marker after which the autoload-input vertical injects a key.
///
/// The autoloaded user-space virtio-input keyboard driver is *interrupt
/// driven*: after `VirtioInput::open` brings the device to `DRIVER_OK` and
/// posts its event-queue buffers, the driver binds its granted device
/// interrupt line through the `irq_bind` syscall and parks on `irq_wait`
/// (`lib/drvrt::RtDriverHost::notify_wait`). `irq_bind` is an audited syscall
/// (`lib/abi` `SyscallSpec { audit: true }`), and **only a user-space driver
/// issues the `irq_bind` *syscall*** — the in-kernel block path binds its
/// completion line through `IrqTable::bind` directly — so this dispatch
/// record appears exactly once, the instant the keyboard driver is armed and
/// waiting. Injecting then guarantees the device is active with posted
/// buffers, so the keypress is delivered (virtio-input interrupts are
/// level-triggered, so the assertion is held until the kernel routes+enables
/// the line on the driver's first park) rather than dropped against an
/// un-ready device. It is the user-space analogue of the in-kernel
/// `input_virtio_mmio` vertical's "eventq armed" readiness marker
/// (inject only once the driver can receive).
const AUTOLOAD_INPUT_KEY_MARKER: &str = "sc=irq_bind";

/// The username line the session-ceiling vertical types at `Username: `.
const SESSION_USERNAME_LINE: &str = "root\n";

/// The password line the session-ceiling vertical types at `Password: `.
const SESSION_PASSWORD_LINE: &str = "root\n";

const _: () = {
    assert!(
        is_line_of(
            UNLOCK_PASSPHRASE_LINE.as_bytes(),
            rustos_test_encrypted_root_image::PASSPHRASE
        ),
        "UNLOCK_PASSPHRASE_LINE drifted from the fixture passphrase"
    );
    assert!(
        is_line_of(
            SESSION_USERNAME_LINE.as_bytes(),
            rustos_test_encrypted_root_image::USERNAME.as_bytes()
        ),
        "SESSION_USERNAME_LINE drifted from the fixture account"
    );
    assert!(
        is_line_of(
            SESSION_PASSWORD_LINE.as_bytes(),
            rustos_test_encrypted_root_image::PASSWORD.as_bytes()
        ),
        "SESSION_PASSWORD_LINE drifted from the fixture account"
    );
};

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
        serial: &[],
    },
    // Stage 3a (b) deliverable: AP bring-up + scheduler stress on real
    // (emulated) cores. The host-side `rustos-test-scheduler-stress`
    // workspace test continues to satisfy the unit / cross-
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the x86_64
    // ring-3 exercise for the Arch HAL "enter user mode" primitive
    // (`kernel/arch/x86_64/src/userentry.rs`, `rustos_arch_api::EnterUser`). Unlike `rustos-test-abi-sys-syscall-qemu`, which
    // issues the same `abi-sys` stub from ring 0 (the x86_64 `syscall`
    // traps identically from any privilege level and never crosses a
    // boundary), this test boots the production kernel and, on
    // `AuditEvent::BootCompleted`, builds a ring-3 address space — a
    // user-accessible, executable, non-writable alias of the
    // `ros_sys_cap_query` stub page (W^X) plus a USER read/write
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the riscv64
    // end-to-end C-program round-trip — the headline CC5 work. The build
    // script builds the Rust crt0 + `ros_sys_*` runtime shim
    // (`tests/integration/cc5_program`) as a PIE `staticlib`, compiles the
    // genuinely C-language program (`cc5_program/csrc/main.c`) with the audited,
    // version-pinned, checksummed `clang`/`ld.lld` wrapper (`tools/cc`), links them into one PIE image, and converts it to an `rxe`
    // blob (`rustos_itest_harness::elf2rxe`) carrying the kernel's syscall CFI
    // tag. On boot the test stands up an Sv39 address space (identity-mapping
    // the kernel + MMIO), installs the trap vector and a dispatch callback, then
    // calls the production capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to build
    // the program's U-mode image and `sret` into it. The C program checks a
    // Time64 value across the pre-1970/post-2038 boundaries, an ipc header,
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
        serial: &[],
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the aarch64
    // end-to-end C-program round-trip — the EL0 analogue of the riscv64
    // vertical above. The build script builds the Rust crt0 + `ros_sys_*`
    // runtime shim (`tests/integration/cc5_program`) as a PIE `staticlib`,
    // compiles the genuinely C-language program (`cc5_program/csrc/main.c`)
    // with the audited, version-pinned, checksummed `clang`/`ld.lld` wrapper
    // (`tools/cc`), links them into one PIE image, and converts
    // it to an `rxe` blob (`rustos_itest_harness::elf2rxe`) carrying the
    // kernel's syscall CFI tag. On boot the test enables `CPACR_EL1.FPEN`,
    // stands up a stage-1 address space (identity-mapping the kernel + MMIO,
    // EL1), installs the EL1 vector table and a dispatch callback, then calls
    // the production capability-checked, audited spawn caller
    // (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's EL0 image and `eret` into it. The C program checks a
    // Time64 value across the pre-1970/post-2038 boundaries, an ipc header,
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
        serial: &[],
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the x86_64
    // end-to-end C-program round-trip — the ring-3 analogue of the
    // riscv64/aarch64 verticals above, completing CC5. The build script builds
    // the Rust crt0 + `ros_sys_*` runtime shim (`tests/integration/cc5_program`)
    // as a PIE `staticlib`, compiles the genuinely C-language program
    // (`cc5_program/csrc/main.c`) with the audited, version-pinned, checksummed
    // `clang`/`ld.lld` wrapper (`tools/cc`), links them into one
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
    // `iretq` into it. The C program checks a Time64 value across the
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // PI Stage RV-P3 (`plans/PI.md`): `rustos-test-spawn-init-qemu-riscv64`
    // boots the *production* riscv64 `rustos-kernel` pipeline
    // (`boot_riscv64::boot`) on the `virt` board, then drops into PID 1
    // (`init`) in U-mode through the `InitSpawn` seam `boot_riscv64` installs
    // into the `BootInfo` hand-off. After `kernel_core::kernel_main` emits
    // `AuditEvent::BootCompleted` it builds the embedded `init` (`Run`)
    // U-mode image through the capability-checked, audited `spawn_image` +
    // `admit_init` (emitting `ProcessSpawned`, `EventId(4030)`) and dispatches
    // it; `init` writes its banner through `stream_write` (over the SBI
    // console backing) and issues the audited `spawn` syscall, whose `ecall`
    // traps back through the S-mode vector to the production dispatch callback
    // (emitting `SyscallInvoked`, `EventId(5000)`). The audit sink reports
    // PASS through the `SiFive` Test finisher once it has seen `ProcessSpawned`
    // then `SyscallInvoked` — proving PID 1 reached U-mode, wrote its banner,
    // and trapped back (the riscv64 sibling of the aarch64 / x86_64
    // `spawn-init-qemu` verticals). Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-spawn-init-qemu-riscv64",
        binary: "rustos-test-spawn-init-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
    // task stalls the drain and the harness reports a timeout (fail-loud). Single CPU and a 60-second budget match the other
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
        serial: &[],
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
    // (fail-loud). Single CPU and a 60-second budget match
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
        serial: &[],
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
    // timeout (fail-loud). Single CPU and a 60-second budget
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (riscv64 stage G1):
    // `rustos-test-stack-guard-qemu-riscv64` is the riscv64 sibling of
    // `rustos-test-stack-guard-qemu-aarch64` — it proves the live Sv39
    // block-split the riscv64 kthread kernel-stack guard page is built on.
    // It builds one `paging::AddressSpace` (identity-maps the low 4 GiB),
    // calls `AddressSpace::split_block` to shatter the coarse identity leaf
    // covering a dedicated `GUARD_PAGE` static down to 4 KiB pages
    // (preserving every mapping), installs the S-mode trap vector + a
    // `fault` handler, turns paging on, writes+reads-back a sentinel
    // through the guard page (proving the split preserved the mapping
    // live), then `unmap`s that single page through the Arch HAL +
    // `flush_page`s its stale TLB entry and reads it: the MMU raises a load
    // page fault, the handler confirms the cause / faulting address, and
    // writes the `SiFive` Test PASS finisher. A regression that fails to
    // split, preserve, or unmap either reports FAILURE explicitly or never
    // faults (timing out). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-stack-guard-qemu-riscv64",
        binary: "rustos-test-stack-guard-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (riscv64 stage G3c): the
    // *production* fault-form, the riscv64 sibling of
    // `rustos-test-stack-overrun-qemu-aarch64`.
    // `rustos-test-stack-overrun-qemu-riscv64` proves that an overrunning
    // kthread takes a synchronous store page fault, not a next-reschedule
    // canary detection. It builds an Sv39 identity `AddressSpace`, prepares
    // a 2 MiB-aligned guard arena (`AddressSpace::prepare_guard_arena`, G2),
    // carves one kthread stack region `[guard page | usable stack]` out of
    // it, installs the S-mode trap vector + a `fault` handler, turns paging
    // on, then `unmap`s the guard page through the Arch HAL + `flush_page`s
    // it — the production guard-page mechanism (G3b-2). It then builds the
    // live `rustos-kernel-sched-eevdf` `Scheduler` over `RiscvArch`, admits a
    // kthread on that stack via `kernel_core::spawn_kthread_with_stack`, and
    // drives the cooperative `step` loop. The kthread body overruns its
    // stack (writes the highest guard byte, the first byte a contiguous
    // downward overrun crosses); because the guard page is unmapped the
    // access raises a synchronous store page fault *while the kthread runs*,
    // the handler confirms the cause / faulting address, and writes the
    // `SiFive` Test PASS finisher. A regression that left the page mapped
    // lets the body return cleanly; the drain loop then reports FAILURE
    // explicitly rather than passing. Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-stack-overrun-qemu-riscv64",
        binary: "rustos-test-stack-overrun-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // SPAWN Stage SP3b (`plans/SPAWN.md`) + `plans/PI.md` P11:
    // `rustos-test-spawn-session-qemu-aarch64` boots the *production*
    // aarch64 `rustos-kernel` pipeline (`boot_aarch64::boot`) on the `virt`
    // board with both the `InitSpawn` seam and the runtime `ProcessSpawn`
    // producer + embedded-program registry installed. After `kernel_main`
    // emits `BootCompleted` it spawns PID 1 `init` into EL0 (`ProcessSpawned`,
    // `EventId(4030)` #1); `init` writes its banner and supervises the
    // session: it issues the audited `spawn` syscall (`SyscallInvoked`,
    // `EventId(5000)` #1) for `/System/Services/login` (P11) and `wait`s on
    // it (`SyscallInvoked` #2). The producer builds login a fresh,
    // hardware-isolated address space (`ProcessSpawned` #2) and admits it
    // Ready; login's `users_db_read` fails closed (no root volume on this
    // board, so no database is held), it wires the deny-all authenticator,
    // writes its `Username: ` prompt and **blocks** in `stream_read` on the
    // kernel-core `BlockingConsoleRead` backing. The runner then holds the
    // scripted dialogue below with it: it types `root`, waits for the
    // `Password: ` prompt (proving login read the username line whole and
    // re-prompted rather than crashing per keystroke — the regression the
    // allocation-free prompt path fixed), types a password the deny-all
    // authenticator refuses (`Login incorrect`), waits for the **second**
    // `Username: ` prompt of the retry loop, and finally types a 513-byte
    // line (one byte past login's 512-byte `LINE_MAX` validation bound); login refuses the over-long line whole, records
    // the console error, and exits fail-closed; `init` reaps it and
    // relaunches it (`ProcessSpawned` #3). The audit sink reports PASS
    // through the ARM semihosting finisher once it has seen three
    // `ProcessSpawned` and four audited syscalls — and the runner fails
    // the run if the guest exits before every scripted prompt appeared
    // and every line was sent, so a login that dies mid-dialogue cannot
    // pass on its relaunch alone. Together that proves the interactive
    // read path delivered real UART RX bytes to the blocked login across
    // a full prompt→reply→re-prompt exchange *and* that supervision
    // (reap + restart) ran. Logging in for real rides the P8/P11
    // root-volume mount; until then every credential check on this board
    // fails closed. Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-spawn-session-qemu-aarch64",
        binary: "rustos-test-spawn-session-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[
            ("Username: ", "root\n"),
            ("Password: ", "wrong\n"),
            (
                "Username: ",
                "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
            ),
        ],
    },
    // PI Design D P-3 (`.junie/next-pi-prompt.md`):
    // `rustos-test-devmgr-hwtree-qemu-aarch64` boots the *production* aarch64
    // `rustos-kernel` pipeline (`boot_aarch64::boot`) verbatim on the `virt`
    // board and proves the **device-manager service's reactive observe loop**
    // end to end. PID 1 `init` now launches the perpetual `devmgr` service
    // (`/System/Services/devmgr`, in `spawn_layout::SPAWN_PROGRAMS`) before the
    // login session; `devmgr` reads the discovered hardware tree
    // (`hw_tree_read`) and **truly parks** in `hw_tree_wait`, registering on the
    // kernel's `HW_TREE_WAITQ` (Design D P-2 — no busy poll). The test injects
    // an observing `HwTreeSource` (the same dependency-injection seam the boot
    // path exposes for the log/audit sinks): the `hw_tree_wait` handler calls
    // its `generation()` in `devmgr`'s own context, after registering and just
    // before parking, so a non-empty `HW_TREE_WAITQ` there is the "devmgr is
    // about to park" witness. On the first park the source appends a node to
    // the authoritative `HwTreeStore` — a real generation bump / simulated
    // hotplug that calls `hw_tree_wake` exactly as the floor bus bring-up does —
    // and on the re-park (devmgr woke, re-read, re-registered) it reports PASS
    // via the ARM semihosting finisher. Because the witness is driven by
    // `devmgr`'s own read/wait loop it needs **no** login dialogue to keep
    // events flowing (an earlier audit-sink-driven version was flaky because
    // that incidental traffic dried up before `devmgr` parked); the run needs
    // no scripted serial input at all. `hw_tree_read`/`hw_tree_wait` are
    // unaudited high-volume reactive syscalls, so the wake's *correctness* is
    // pinned by the host unit tests (`kernel/core/src/waitq.rs`,
    // `kernel/core/src/syscalls.rs`); this vertical proves the integrated
    // boot → spawn → read → park → real-generation-bump → no-starvation path on
    // the production pipeline. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-devmgr-hwtree-qemu-aarch64",
        binary: "rustos-test-devmgr-hwtree-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
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
    // page-table root) and drains the `step` loop; the dispatch callback
    // maps each task's `yield`/`exit` `svc` to `reschedule_current`, suspending
    // the running task back to the dispatcher exactly as the production callback
    // does. PASS once both tasks yielded their full count and exited; a switch
    // that never resumes stalls the drain and the harness times out (fail-loud). Single CPU and a 60-second budget match the other
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
        serial: &[],
    },
    // PI Stage D2b-2b-A P-1 (`plans/PI.md`): the aarch64 involuntary-preemption
    // vertical — the proof that the production generic-timer IRQ preempts a
    // **runaway** EL0 task on the `virt` board (the P-1a behavioural test the
    // boot matrix only covered by non-regression). It reads the GICv2 base +
    // timer rate from the embedded `virt` DTB (P3/P4), brings up the EL1 vectors
    // + GICv2, and builds **one** hardware-isolated EL0 address space from the
    // pure-Rust `rustos-test-el0-spinner` fixture (a `black_box`-guarded busy
    // loop that issues no syscall, built PIE + converted to `rxe` by `build.rs`)
    // through the capability-checked, audited `kernel_core::spawn_image`. It
    // then arms the **production** preemption path verbatim (the `rustos_arch_aarch64::preempt` surface the bin crate's
    // `arm_preemption` uses): a per-CPU `PreemptStorage`, an EL0-preemption
    // callback that `reschedule_current(_, Yield)`s the running task, and the
    // periodic generic timer; EL0 runs preemptible (`SPSR_EL0T_PREEMPTIBLE`), so
    // a tick taken while the spinner runs traps to `LOWER_IRQ` and preempts it.
    // Because the loop never traps, the only way it leaves EL0 before its final
    // `exit` is an involuntary preemption. PASS once the preempt callback fired
    // at least once AND the task — resumed mid-loop after each preemption —
    // still completed and exited; a preemption that never fires (the `step`
    // spins forever inside EL0) or a botched resume (the task never exits)
    // times out (fail-loud). Single CPU; a 120-second budget
    // covers the multi-tick busy loop under QEMU TCG.
    QemuTest {
        package: "rustos-test-preempt-el0-qemu-aarch64",
        binary: "rustos-test-preempt-el0-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage D2b-2b-A P-1b (`plans/PI.md`): the riscv64 involuntary-preemption
    // vertical — the cross-port sibling of the aarch64 preempt test, proving the
    // production supervisor-timer interrupt preempts a **runaway** U-mode task on
    // the `virt` board. It reads the `timebase-frequency` from the firmware DTB
    // (the `a1` pointer), installs the S-mode trap vector via
    // `trap::install_trap_vector` — NOT `init_traps`, so `sstatus.SIE` stays
    // clear and the kernel itself is never preempted — and builds **one**
    // hardware-isolated Sv39 U-mode address space from the pure-Rust
    // `rustos-test-el0-spinner` fixture (a `black_box`-guarded busy loop that
    // issues no syscall, built PIE + converted to `rxe` by `build.rs`) through
    // the capability-checked, audited `kernel_core::spawn_image`. It then arms
    // the **production** preemption path verbatim (the
    // `rustos_arch_riscv64::preempt` surface the bin crate's `arm_preemption`
    // uses): a per-hart `PreemptStorage`, a U-mode-preemption callback that
    // `reschedule_current(_, Yield)`s the running task, and the periodic SBI
    // timer (`init_local_preempt` sets `sie.STIE`). A supervisor-timer interrupt
    // is taken while the spinner runs in U-mode by the privilege rule U < S, so
    // the trap handler's SPP-gated preempt point fires. Because the loop never
    // traps, the only way it leaves U-mode before its final `exit` is an
    // involuntary preemption. PASS once the preempt callback fired at least once
    // AND the task — resumed mid-loop after each preemption — still completed
    // and exited; a preemption that never fires (the `step` spins forever inside
    // U-mode) or a botched resume (the task never exits) times out (fail-loud). Single CPU; a 120-second budget covers the multi-tick
    // busy loop under QEMU TCG.
    QemuTest {
        package: "rustos-test-preempt-el0-qemu-riscv64",
        binary: "rustos-test-preempt-el0-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage D2b-2b-A P-1c (`plans/PI.md`): the x86_64 involuntary-preemption
    // vertical — the cross-port sibling of the aarch64/riscv64 preempt tests,
    // proving the production LAPIC-timer interrupt preempts a **runaway** ring-3
    // task. Unlike the other ports, the ring-3 transition needs the GDT ring-3
    // selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so the
    // test boots the production `rustos-kernel` pipeline (which also programs
    // the periodic LAPIC timer in `preempt::init_local_preempt`); only the audit
    // sink is replaced. On `BootCompleted` it enables `IA32_EFER.NXE`, builds
    // **one** hardware-isolated ring-3 address space from the pure-Rust
    // `rustos-test-el0-spinner` fixture (a `black_box`-guarded busy loop that
    // issues no syscall, built PIE + converted to `rxe` by `build.rs`) through
    // the capability-checked, audited `kernel_core::spawn_image`, and admits it
    // as a resumable user kthread whose `pre_resume` hook reloads CR3 and
    // repoints **both** the per-CPU `syscall` entry stack
    // (`syscall_entry::set_kernel_rsp0`) and the `TSS.RSP0` trap stack
    // (`percpu::install_tss_rsp0`) at the task's own kernel stack. It then arms
    // the **production** ring-3-preemption path verbatim (the
    // `rustos_arch_x86_64::preempt::set_preempt_callback` surface the bin crate's
    // `install_irq_dispatch` uses): a callback that `reschedule_current(_,
    // Yield)`s the running task. Ring 3 runs preemptible (`userentry`'s `IF`-set
    // `RFLAGS`), so a LAPIC-timer tick taken while the spinner runs lands on the
    // timer ISR and (gated on the saved `CS` RPL) drives the preempt point.
    // Because the loop never traps, the only way it leaves ring 3 before its
    // final `exit` is an involuntary preemption. PASS once the preempt callback
    // fired at least once AND the task — resumed mid-loop after each preemption —
    // still completed and exited; a preemption that never fires (the `step`
    // spins forever inside ring 3) or a botched resume (the task never exits)
    // times out (fail-loud). Single CPU; a 120-second budget
    // covers the multi-tick busy loop under QEMU TCG.
    QemuTest {
        package: "rustos-test-preempt-el0-qemu-x86-64",
        binary: "rustos-test-preempt-el0-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PLAN.md P-5 (2026-06-23 amendment): the aarch64
    // in-kernel interrupt-delivery / non-preemption vertical — the dual of the
    // `preempt-el0` tests. Where those prove a runaway **EL0** task IS
    // involuntarily preempted, this proves the property the serial-stall saga
    // turned on: a busy **in-kernel** kthread that issues no `yield` and no
    // syscall still takes the generic-timer IRQ *during* its span (the EL1 IRQ
    // path runs `on_timer_interrupt` and the tick callback records it), but
    // because the tick was taken from EL1 the running task is NOT rescheduled
    // (the kernel is non-preemptible), so the EL0-preemption callback never
    // fires. It reads the GICv2 base + timer rate from the embedded `virt` DTB,
    // brings up the EL1 vectors + GICv2, registers the production
    // `rustos_arch_aarch64::preempt` surface verbatim (a
    // per-CPU `PreemptStorage`, the EL0-preemption callback, a timer-tick
    // callback, and the enabled generic-timer PPI), builds a live eevdf
    // `Scheduler`, admits one in-kernel kthread that arms the timer one-shot and
    // busy-loops, and enables device IRQs at the PE (`exceptions::enable_irq`,
    // the aarch64 backing of `KernelArch::set_device_irqs(true)`). PASS once a
    // tick was taken during the busy span AND the EL0-preemption callback fired
    // zero times AND the kthread resumed and ran to its voluntary completion.
    // Under the old cooperative loop (device IRQs masked across the whole task
    // run) no tick would ever be taken and the kthread would spin forever, so
    // the run fails loudly — a failure finisher or the harness timeout. Single CPU; a 120-second budget covers the busy loop
    // under QEMU TCG.
    QemuTest {
        package: "rustos-test-preempt-inkernel-qemu-aarch64",
        binary: "rustos-test-preempt-inkernel-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PLAN.md Stage 4.HW: the aarch64 driver-spawn handshake vertical — the
    // proving slice of the kernel-side production driver spawner. The build
    // script compiles the pure-Rust driver-stub fixture
    // (`rustos-test-driver-register-program`) PIE and converts it to an
    // `rxe` blob carrying the kernel's syscall CFI tag, registered under a
    // `/System/Drivers/` path. On boot the test discovers the board from the
    // embedded `virt` DTB, enables the identity MMU + EL1 vectors, builds a
    // live `kernel/mem` FrameAllocator, binds the reply Port (send-gated on
    // a driver-class capability) into a live `RwLock<PortRegistry>`,
    // installs the production `KernelDispatchHook` through a
    // `DispatchCallbackSlot`, and spawns the stub through the production
    // parameterised `Aarch64ProcessSpawn::spawn_with` via the exported
    // `KernelSpawnCtx` admit path — driver-class caps plus the reply
    // endpoint id in `arg(1)`, exactly the hand-off the driver host gives a
    // spawned driver. The host side drives the cooperative `step` loop,
    // polling `Port::recv` under a bounded budget; the stub reads `arg(1)`,
    // sends `DriverRegisterReply::registered(...)` over the production
    // `ipc_send` path (caller-context resolution, copy-in, capability-gated
    // `Port::send`), and exits. PASS once the fail-closed-decoded reply
    // round-trips the stub's pinned handle; any shortfall writes a distinct
    // failure finisher or times out (fail-loud). Single CPU
    // and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "rustos-test-driver-spawn-qemu-aarch64",
        binary: "rustos-test-driver-spawn-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // plans/USB.md U1: the aarch64 driver-*unload* vertical — the symmetric
    // partner of the driver-spawn handshake above. It reuses the same signed
    // driver-stub fixture and the production devmgr-driven autoload/spawn path
    // (discover the `virt` board, build the live registries, `DeviceManager::
    // autoload` through `SpawnDriverLoader` + `InitCtxDriverProcessSpawn` over
    // `Aarch64ProcessSpawn::spawn_with`), so the driver is admitted Ready with
    // its capability record + address-space-registry entry minted. It then
    // drives the production unload mechanism `InitSpawnCtx::
    // terminate_driver_process` (the seam the driver-store server runs for
    // `StoreRequest::Unload`) and asserts the scheduler task was reaped
    // (live-task count 1→0) and its caps + address space reclaimed, and that a
    // second unload of the now-gone handle fails closed with `NotFound`
    // (idempotent). PASS once teardown reclaimed everything; any shortfall
    // writes a distinct failure finisher or times out (fail-loud). The driver
    // is never dispatched, so it issues no syscall and needs no reply port.
    // Single CPU and a 60-second budget match the driver-spawn vertical.
    QemuTest {
        package: "rustos-test-driver-unload-qemu-aarch64",
        binary: "rustos-test-driver-unload-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // SPAWN Stage SP5b-2 (`plans/SPAWN.md` §1): the aarch64 `mem_map`/
    // `mem_unmap` vertical — the first proof that an EL0 process obtains and
    // releases anonymous `RW` memory at runtime via `abi-v1`, on the `virt`
    // board. It reads the GICv2 base + timer rate from the embedded `virt` DTB
    // (P3/P4), brings up the EL1 vectors + GICv2, and builds **one** hardware-
    // isolated EL0 address space from the pure-Rust `rustos-test-mem-map`
    // fixture (built PIE + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`. It **retains** that
    // space live behind a `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, admits the program as a
    // resumable user kthread (`spawn_user_kthread`), and routes the
    // program's `mem_map`/`mem_unmap` `svc`s through the producer. The fixture
    // maps a region (FIXED), writes+verifies a pattern, unmaps it, then touches
    // the released range; the fault handler reports the use-after-unmap data
    // abort as PASS. A verification failure exits early (a distinct finisher)
    // and a missing fault stalls the drain (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "rustos-test-mem-map-qemu-aarch64",
        binary: "rustos-test-mem-map-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage 5d-0-ii (b′)-2 (`plans/PI.md`): the aarch64 `mmio_map` vertical —
    // the first proof that an EL0 driver maps a **granted device MMIO window**
    // at runtime via `abi-v1` `mmio_map` over the per-task **retained live
    // address space**, on the `virt` board. It reads the GICv2 base + timer
    // rate from the embedded `virt` DTB (P3/P4), brings up the EL1 vectors +
    // GICv2, and builds **one** hardware-isolated EL0 address space from the
    // pure-Rust `rustos-test-mmio-map` fixture (built PIE + converted to `rxe`
    // by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It wraps that space in the production
    // `kernel_mem::LiveSpace` and admits the program through the production
    // `kernel_core::spawn_user_kthread_with_stack_live`, so the retained space
    // is published on the per-CPU live-space slot while the program runs
    // (exactly the production aarch64 spawn path). It mints the task a grant
    // for the first `virt` virtio-MMIO transport window and routes the
    // program's `mmio_map` `svc` through `with_current_live_space` +
    // `LiveSpace::map_device_window`; the program reads the device's
    // `MagicValue` register (`0x74726976`) back through the mapped, caching-
    // disabled window and exits 0, which the dispatch callback reports as PASS.
    // A refused map, the wrong register value, an unexpected syscall, or no
    // exit trips a distinct finisher or times out (fail-loud).
    // The registry-backed grant owner-check is host-proven in
    // `kernel/core`. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-mmio-map-qemu-aarch64",
        binary: "rustos-test-mmio-map-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // SPAWN Stage SP5b-2 (`plans/SPAWN.md` §1): the riscv64 `mem_map`/
    // `mem_unmap` vertical — the riscv64 sibling of the aarch64 vertical above,
    // proving a U-mode process obtains and releases anonymous `RW` memory at
    // runtime via `abi-v1` on the `virt` board. It stands up an Sv39 address
    // space (identity-mapping the kernel + MMIO), activates `satp`, installs
    // the trap vector + a dispatch callback + a fault handler, and builds
    // **one** hardware-isolated U-mode address space from the same pure-Rust
    // `rustos-test-mem-map` fixture (built PIE + converted to `rxe` by
    // `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It **retains** that space live behind a
    // `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, then `sret`s straight into
    // the program (no scheduler — the single task only direct-returns from its
    // `ecall`s, so the riscv64 cooperative-switch trap-save path is not on the
    // critical path); the dispatch callback routes the program's
    // `mem_map`/`mem_unmap` `ecall`s through the producer. The fixture maps a
    // region (FIXED), writes+verifies a pattern, unmaps it, then touches the
    // released range; the fault handler reports the use-after-unmap page fault
    // as PASS. A verification failure exits early (a distinct finisher) and a
    // missing fault stalls (fail-loud). Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-mem-map-qemu-riscv64",
        binary: "rustos-test-mem-map-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage RV-X1 (`plans/PI.md` §X tail): the riscv64 single-resumable-
    // user-kthread vertical — the first proof that a U-mode task is admitted as
    // a *resumable* user kthread on riscv64 and cooperatively parks/resumes
    // under the live scheduler over the RV1 park-safe trap path, the cross-port
    // sibling of the x86_64 X1 vertical and the aarch64 SP2c timeshare (one
    // task; the two-task `sscratch` per-task repointing is RV-X2). On boot it
    // reads the generic-timer rate from the firmware device tree, stands up an
    // Sv39 address space (identity-mapping the kernel + MMIO), activates `satp`,
    // and installs the trap vector + a dispatch callback. It builds **one**
    // hardware-isolated U-mode address space from the pure-Rust
    // `rustos-test-el0-yielder` fixture (built PIE + converted to `rxe` by
    // `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`, and admits it via `spawn_user_kthread`. The
    // task's `pre_resume` hook reactivates the task's own `satp` root
    // (`paging::activate_user_root`, the RV-X1 primitive). The cooperative
    // `step` loop drives it; the dispatch callback maps each `yield`/`exit`
    // `ecall` to `reschedule_current`, so it ping-pongs with the dispatcher on
    // its own kernel stack. PASS once it yielded its full count and exited; a
    // wrong drain count, an unexpected syscall, or a stall flips
    // `qemu_exit::exit_failure` or times out (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // riscv64 tests.
    QemuTest {
        package: "rustos-test-spawn-el0-resume-qemu-riscv64",
        binary: "rustos-test-spawn-el0-resume-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage RV-X2 (`plans/PI.md` §X tail): the riscv64 two-task EL0
    // timeshare vertical — the first proof that TWO U-mode tasks timeshare one
    // hart as resumable user kthreads on riscv64 under the live scheduler over
    // the RV1 park-safe trap path, the cross-port sibling of the x86_64 X2
    // vertical and the aarch64 SP2c timeshare. On boot it reads the
    // generic-timer rate from the firmware device tree, installs the trap vector
    // + a dispatch callback, and builds **two** hardware-isolated U-mode address
    // spaces (two `PageTablePool`s + a shared frame pool) from
    // the pure-Rust `rustos-test-el0-yielder` fixture (built PIE + converted to
    // `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`, and admits each via `spawn_user_kthread`. Each
    // task's `pre_resume` hook reactivates its own `satp` root
    // (`paging::activate_user_root`); `sscratch` is per-task hardware state that
    // `userentry::enter_user` arms on first entry and the RV1 trap vector
    // re-arms from each task's own kernel-stack frame on every U-return, so no
    // dispatcher-side stack repointing is needed (unlike x86_64's per-CPU
    // `set_kernel_rsp0`). The cooperative `step` loop drives both; the dispatch
    // callback maps each `yield`/`exit` `ecall` to `reschedule_current`, so the
    // two ping-pong with the dispatcher on their own kernel stacks. PASS once
    // both yielded their full count and exited; a wrong drain count, an
    // unexpected syscall, or a stall flips `qemu_exit::exit_failure` or times
    // out (fail-loud). Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-spawn-el0-timeshare-qemu-riscv64",
        binary: "rustos-test-spawn-el0-timeshare-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage RV-X3 (`plans/PI.md` §X tail): the riscv64 runtime-`spawn`
    // concurrent-producer vertical — the cross-port sibling of
    // `spawn_session_qemu_aarch64` / `_x86_64`, proving a parent task's
    // `CAP_PROC_SPAWN`-gated `spawn` builds a fresh, hardware-isolated Sv39
    // child space and admits it Ready concurrently on the riscv64 `virt` board.
    // The build script compiles the pure-Rust `rustos-test-spawn-session-program`
    // fixture twice (the parent role and the child/session role, built PIE +
    // converted to `rxe`). On boot it reads the generic-timer rate from the
    // firmware device tree, installs the trap vector + a dispatch callback,
    // builds the parent a hardware-isolated Sv39 U-mode space via
    // `kernel_core::spawn_image` (capability-checked + audited), and admits it
    // via `spawn_user_kthread` onto a leaked-`'static` live scheduler. The
    // parent issues a real `spawn` `ecall`; the dispatch callback routes it to a
    // riscv64 `ProcessSpawn` producer that builds the child a fresh isolated
    // Sv39 space THROUGH THE PARENT'S IDENTITY WINDOW WITHOUT switching the
    // running parent's `satp` and admits it Ready concurrently. The callback
    // maps each `yield`/`exit` `ecall` to `reschedule_current`, so the parent
    // and child timeshare the hart on their own kernel stacks (the RV1 park-safe
    // path). PASS once the producer built the child and both tasks ran to
    // `exit`; a failed spawn, an unexpected syscall, a wrong drain count, or a
    // stall flips `qemu_exit::exit_failure` or times out (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-spawn-session-qemu-riscv64",
        binary: "rustos-test-spawn-session-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // SPAWN Stage SP5b-2 (`plans/SPAWN.md` §1): the x86_64 `mem_map`/
    // `mem_unmap` vertical — the x86_64 sibling of the aarch64/riscv64
    // verticals above, proving a ring-3 process obtains and releases anonymous
    // `RW` memory at runtime via `abi-v1`. Unlike those self-contained test
    // kernels the x86_64 ring-3 transition needs the GDT user selectors, the
    // TSS, and `syscall`/`IA32_LSTAR` entry, so it boots the production
    // `rustos-kernel` pipeline (like `spawn_program_qemu_x86_64`); that
    // pipeline now also installs the dedicated, error-code-aware page-fault
    // entry (`rustos_arch_x86_64::fault`), so the deliberate use-after-unmap
    // `#PF` is observable. On `BootCompleted` it enables `IA32_EFER.NXE`,
    // installs a `fault` observer, builds **one** hardware-isolated user
    // address space from the same pure-Rust `rustos-test-mem-map` fixture
    // (built PIE + converted to `rxe` by `build.rs`) through the capability-
    // checked, audited `kernel_core::spawn_image`, **retains** it live behind a
    // `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, and `iretq`s into it; the
    // dispatch callback routes the program's `mem_map`/`mem_unmap` `syscall`s
    // through the producer. The fixture maps a region (FIXED), writes+verifies
    // a pattern, unmaps it, then touches the released range; the fault observer
    // reports the use-after-unmap `#PF` as PASS. A verification failure, an
    // unexpected syscall, or a missing fault flips `qemu_exit::exit_failure` or
    // times out (fail-loud). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-mem-map-qemu-x86_64",
        binary: "rustos-test-mem-map-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage G1/G2 (`plans/PI.md`): the x86_64 guard-page fault-form
    // vertical — the proof that x86_64, the last `BlockSplit::Pending` port,
    // is now `BlockSplit::Supported`, the sibling of
    // `stack_guard_qemu_{aarch64,riscv64}`. Unlike those self-contained test
    // kernels, x86_64 long-mode bring-up (GDT, the dedicated error-code-aware
    // `#PF` entry, the bump heap) is the production boot pipeline's job, so it
    // boots the real `rustos-kernel` pipeline (like the x86_64 `mem_map`
    // vertical) and does the split / unmap / fault work on `BootCompleted`. It
    // builds a 4 GiB-identity `paging::AddressSpace`, activates it (CR3),
    // `split_block`s the 2 MiB huge page covering a dedicated guard static
    // (reached through its low-identity physical alias), proves the split
    // preserved the mapping (sentinel write/read-back), then `unmap`s +
    // `flush_page`s the single guard page and reads it — the
    // `rustos_arch_x86_64::fault` observer reports the supervisor not-present
    // `#PF` on exactly that page as PASS. A split/unmap failure, a read that
    // does not fault, or a fault elsewhere flips `qemu_exit::exit_failure` or
    // times out (fail-loud). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-stack-guard-qemu-x86_64",
        binary: "rustos-test-stack-guard-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage G3c (`plans/PI.md`): the x86_64 production guard-page
    // fault-form vertical — the proof that an *overrunning kthread* faults
    // synchronously in hardware under the live scheduler, the sibling of
    // `stack_overrun_qemu_aarch64`. Like the x86_64 `stack_guard` vertical it
    // boots the real `rustos-kernel` pipeline (so the GDT, the dedicated
    // error-code-aware `#PF` entry, and the bump heap are installed) and does
    // the work on `BootCompleted`: it builds a 4 GiB-identity
    // `paging::AddressSpace`, activates it (CR3), re-expresses a 2 MiB guard
    // arena at 4 KiB granularity (`prepare_guard_arena`), `unmap`s +
    // `flush_page`s one kthread stack's guard page, builds the live
    // `rustos-kernel-sched-eevdf` `Scheduler` over `X86_64Arch`, and admits a
    // kthread on that arena stack via `spawn_kthread_with_stack`. The
    // kthread's overrun into the unmapped guard page raises a supervisor
    // not-present `#PF`; the `rustos_arch_x86_64::fault` observer confirms the
    // cause + faulting address and reports PASS. A body that returns without
    // faulting (guard regression) drains the loop and flips
    // `qemu_exit::exit_failure`, or times out (fail-loud).
    // Single CPU and a 60-second budget match the other boot-then-do-fixed-
    // work x86_64 tests.
    QemuTest {
        package: "rustos-test-stack-overrun-qemu-x86_64",
        binary: "rustos-test-stack-overrun-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage X1 (`plans/PI.md` §X): the x86_64 single-resumable-user-kthread
    // vertical — the first proof that a ring-3 task is admitted as a *resumable*
    // user kthread on x86_64 and cooperatively parks/resumes under the live
    // scheduler, the cross-port sibling of the aarch64 SP2c timeshare (one task;
    // the two-task `gs:8` durable-save hazard is X2). Like the x86_64 `mem_map`
    // vertical it boots the production `rustos-kernel` pipeline (so the GDT user
    // selectors, the TSS, and `syscall`/`IA32_LSTAR` entry are installed). On
    // `BootCompleted` it enables `IA32_EFER.NXE`, builds **one** hardware-
    // isolated user address space from the pure-Rust `rustos-test-el0-yielder`
    // fixture (built PIE + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`, and admits it via
    // `spawn_user_kthread`. The task's `pre_resume` hook reloads CR3
    // (`paging::activate_user_root`) and repoints the per-CPU `syscall` entry
    // stack at *this* task's own kernel stack (`syscall_entry::set_kernel_rsp0`,
    // the X1 primitive the kthread seam hands the stack top to). The cooperative
    // `step` loop drives it; the dispatch callback maps each `yield`/`exit`
    // `syscall` to `reschedule_current`, so it ping-pongs with the dispatcher on
    // its own kernel stack. PASS once it yielded its full count and exited; a
    // wrong drain count, an unexpected syscall, or a stall flips
    // `qemu_exit::exit_failure` or times out (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work x86_64
    // tests.
    QemuTest {
        package: "rustos-test-spawn-el0-resume-qemu-x86-64",
        binary: "rustos-test-spawn-el0-resume-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage X2 (`plans/PI.md` §X): the x86_64 two-task EL0 timeshare — the
    // cross-port sibling of the aarch64 SP2c timeshare, and the exerciser for
    // the two X2 structural fixes a concurrent mid-handler park needs: (1) the
    // durable user-`%rsp` save moved onto each task's own kernel-stack frame in
    // `syscall_entry_stub` (a concurrent task's syscall entry no longer
    // clobbers a parked task's saved user stack pointer through the shared
    // per-CPU `gs:8` slot), and (2) the `ContextSwitch::enter`/
    // `leave_cooperative_park` `swapgs` balance around the cooperative
    // mid-handler park in `kernel/core`'s kthread runtime (a parked task's entry
    // `swapgs` is balanced before the dispatcher enters a *different* task, so
    // the next ring-3 entry never observes an unbalanced GS-swap and `#DF`s).
    // Like the x86_64 `mem_map`/X1 verticals it boots the production
    // `rustos-kernel` pipeline (so the GDT user selectors, the TSS, and
    // `syscall`/`IA32_LSTAR` entry are installed). On `BootCompleted` it enables
    // `IA32_EFER.NXE`, builds **two** hardware-isolated user address spaces (two
    // PML4s, one shared frame pool) from the pure-Rust `rustos-test-el0-yielder`
    // fixture (built PIE + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`, and admits each
    // via `spawn_user_kthread`. Each task's `pre_resume` hook reloads CR3
    // (`paging::activate_user_root`) and repoints the per-CPU `syscall` entry
    // stack at *this* task's own kernel stack (`syscall_entry::set_kernel_rsp0`).
    // The cooperative `step` loop drives them; the dispatch callback maps each
    // `yield`/`exit` `syscall` to `reschedule_current`, so the two tasks
    // ping-pong with the dispatcher on their own kernel stacks. PASS once both
    // yielded their full count and exited; a wrong drain count, an unexpected
    // syscall, or a stall flips `qemu_exit::exit_failure` or times out
    // (fail-loud). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-spawn-el0-timeshare-qemu-x86-64",
        binary: "rustos-test-spawn-el0-timeshare-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage X3a (`plans/PI.md` §X): the x86_64 PID 1 (`init`) ring-3
    // bring-up vertical — the cross-port sibling of the aarch64
    // `spawn-init-qemu-aarch64` (P6c-3), proving the production x86_64 boot
    // pipeline reaches ring 3 through the real `kernel_main` + `InitSpawn`
    // path (not a test-driven ad-hoc scheduler like X1/X2). It reuses
    // `rustos_kernel::boot`, which now installs the x86_64 PID 1 spawn seam
    // (`init_spawn_x86_64`, via `BootInfo::with_init`) and the COM1 console
    // backing (`BootInfo::with_consoles`); only the audit sink is replaced.
    // After `BootCompleted`, `kernel_main` builds `init`'s ring-3 image
    // through the capability-checked, audited `kernel_core::spawn_image`
    // (emitting `ProcessSpawned`, EventId 4030) and admits it as a resumable
    // user kthread, then drains the run queue. PID 1 `init` writes its gated
    // banner to fd 1 over the COM1 backing, then issues its (audited) `spawn`
    // syscall (EventId 5000; the runtime producer is X3b, so it fails closed)
    // and `exit`s. PASS once a `ProcessSpawned` and an audited `SyscallInvoked`
    // are observed — proving PID 1 reached and executed in ring 3 (the gated
    // banner landed before the audited syscall). A bad image, an entry fault,
    // or an unhandled first `syscall` never emits the audited syscall, so the
    // run times out (fail-loud). Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-spawn-init-qemu-x86-64",
        binary: "rustos-test-spawn-init-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage X3b + X4 follow-on (`plans/PI.md` §X): the x86_64 runtime
    // `spawn` concurrent producer **and** `init` session-supervision vertical —
    // the cross-port sibling of the aarch64 `spawn-session-qemu-aarch64`,
    // proving PID 1 `init` launches a second, hardware-isolated process under
    // the live scheduler and then reaps+relaunches it. It reuses
    // `rustos_kernel::boot`, which installs the runtime `ProcessSpawn` producer
    // + embedded-program registry (`spawn_producer_x86_64`, via
    // `BootInfo::with_spawn`) beside the X3a `with_init` seam and the COM1
    // console backing; only the audit sink is replaced. After `BootCompleted`,
    // `kernel_main` builds PID 1 `init`'s ring-3 image (`ProcessSpawned`,
    // EventId 4030, #1) and drains the run queue. `init` writes its gated
    // banner, then issues its (audited) `spawn` syscall for
    // `/Apps/Shell.app/Run`; the producer builds the session a fresh isolated
    // PML4 (`ProcessSpawned` #2) and admits it Ready, then `init` `wait`s on
    // it; the cooperative drain runs the session, which writes its prompt,
    // reads end-of-input (no input backing), and `exit`s; `init`'s `wait` reaps
    // it, returns to ring 3, and **relaunches** the session (`ProcessSpawned`
    // #3). PASS keys on **three** `ProcessSpawned` and **four** audited
    // `SyscallInvoked` (EventId 5000 — `init`'s `spawn`, the session's `exit`,
    // `init`'s `wait`, and `init`'s relaunch `spawn`), proving the full
    // `wait`→reap→relaunch supervision cycle on x86_64. (The earlier 2/2
    // assertion was raised once the X4 follow-on frame-allocator defect was
    // fixed — the boot path now reserves the kernel image out of usable RAM, so
    // the relaunch producer no longer corrupts the kernel; see boot.rs
    // `build_memory_map` and `plans/PI.md` §X.) A regression that never builds,
    // runs, reaps, or relaunches the session never reaches the threshold, so
    // the run times out (fail-loud). Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-spawn-session-qemu-x86-64",
        binary: "rustos-test-spawn-session-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage P6e-3b prerequisite (`plans/PI.md`): the aarch64 heap-allocator
    // vertical — the proof that the `rustos-rt` `mem_map`-backed
    // `#[global_allocator]` works end to end in an EL0 process on the `virt`
    // board, so a first-party Rust program can use `alloc` (`Box`/`Vec`/
    // `String`) before the shell REPL is wired in. It reads the GICv2 base +
    // timer rate from the embedded `virt` DTB (P3/P4), brings up the EL1
    // vectors + GICv2, and builds **one** hardware-isolated EL0 address space
    // from the pure-Rust `rustos-test-heap` fixture (built PIE + converted to
    // `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It **retains** that space live behind a
    // `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, admits the program as a
    // resumable user kthread (`spawn_user_kthread`), and routes the
    // program's allocator-issued `mem_map`/`mem_unmap` `svc`s through the
    // producer. The fixture Box-allocates, grows a `Vec` across several pages,
    // reallocates after freeing, verifies every value, and exits 0 — reported
    // as PASS. A non-zero exit, an unexpected syscall, or a fault writes a
    // distinct failure finisher; a stall times out (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-heap-qemu-aarch64",
        binary: "rustos-test-heap-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // SPAWN Stage SP6b (`plans/SPAWN.md` §1): the aarch64 `wait` vertical —
    // the proof that a parent process can block on, reap, and read back the
    // exit code of its own child under the live scheduler on the `virt` board.
    // It reads the GICv2 base + timer rate from the embedded `virt` DTB
    // (P3/P4), brings up the EL1 vectors + GICv2, and builds **two** hardware-
    // isolated EL0 address spaces — a child and a parent — from the pure-Rust
    // `rustos-test-wait` fixture (built PIE in both roles + converted to `rxe`
    // by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, admits each as a resumable
    // user kthread (`spawn_user_kthread`), and routes the child's `exit`
    // and the parent's `wait`/`exit` `svc`s through the producer +
    // `reschedule_current`: the producer parks the parent until the child is
    // reapable, then the kernel copies the reaped exit code out to the parent's
    // `status` pointer. PASS once the parent reaped the child, read back the
    // agreed code, and exited 0; a wrong code, a missing reap, an unexpected
    // syscall, or a stall writes a distinct failure finisher (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-wait-qemu-aarch64",
        binary: "rustos-test-wait-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // SPAWN Stage SP7b (`plans/SPAWN.md` §1): the aarch64 `signal` vertical —
    // the proof that a parent process can deliver a control signal
    // (`Signal::Terminate`) to its own child under the live scheduler on the
    // `virt` board. It reads the GICv2 base + timer rate from the embedded
    // `virt` DTB (P3/P4), brings up the EL1 vectors + GICv2, and builds **two**
    // hardware-isolated EL0 address spaces — a child and a parent — from the
    // pure-Rust `rustos-test-signal` fixture (built PIE in both roles +
    // converted to `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It admits the child, threads its
    // scheduler-assigned PID into the parent's startup arguments, records the
    // parent/child link with a `kernel_core::KernelProcessWait` producer,
    // installs a `kernel_core::KernelProcessSignal` producer over that
    // bookkeeping + the live scheduler, admits the parent as a resumable user
    // kthread (`spawn_user_kthread`), and routes the child's `yield` and the
    // parent's `signal`/`wait`/`exit` `svc`s through the producers +
    // `reschedule_current`: the signal producer terminates the child on the
    // scheduler and records the 128+n status, then the parent reaps it and the
    // kernel copies the status out to the parent's `status` pointer. PASS once
    // the parent terminated the child, read back the signalled status, and
    // exited 0; a wrong status, a missing reap, an unexpected syscall, or a
    // stall writes a distinct failure finisher (fail-loud). Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-signal-qemu-aarch64",
        binary: "rustos-test-signal-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage X4 (`plans/PI.md`): the x86_64 `wait` vertical — the cross-port
    // sibling of the aarch64 `wait_qemu_aarch64`, proving a parent ring-3
    // process can block on, reap, and read back the exit code of its own child
    // under the live scheduler on x86_64. It boots the production
    // `rustos-kernel` pipeline (so the GDT ring-3 selectors, the TSS, and
    // `syscall`/`IA32_LSTAR` entry are installed) and, on
    // `AuditEvent::BootCompleted`, builds **two** hardware-isolated ring-3
    // address spaces — a child and a parent — from the pure-Rust
    // `rustos-test-wait` fixture (built PIE in both roles + converted to `rxe`
    // by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, admits each as a resumable
    // user kthread (`spawn_user_kthread`), and routes the child's `exit`
    // and the parent's `wait`/`exit` syscalls through the producer +
    // `reschedule_current`: the producer parks the parent until the child is
    // reapable (exercising the resume-after-cooperative-park return-state path
    // on the x86_64 trap), then the kernel copies the reaped exit code out to
    // the parent's `status` pointer. PASS once the parent reaped the child,
    // read back the agreed code, and exited 0; a wrong code, a missing reap, an
    // unexpected syscall, or a stall writes a distinct failure finisher
    // (fail-loud). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "rustos-test-wait-qemu-x86-64",
        binary: "rustos-test-wait-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // PI Stage RV-X4 (`plans/PI.md` §X tail): the riscv64 `wait` vertical —
    // the cross-port sibling of the aarch64 `wait_qemu_aarch64` / x86_64
    // `wait_qemu_x86_64`, proving a parent U-mode process can block on, reap,
    // and read back the exit code of its own child under the live scheduler on
    // the riscv64 `virt` board. The build script compiles the pure-Rust
    // `rustos-test-wait` fixture twice (child + parent roles, built PIE +
    // converted to `rxe`). On boot it reads the generic-timer rate from the
    // live OpenSBI device tree, installs the trap vector + a dispatch callback,
    // and builds **two** hardware-isolated Sv39 U-mode address spaces — a child
    // and a parent — through the capability-checked, audited
    // `kernel_core::spawn_image`. It records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, admits each as a resumable
    // user kthread (`spawn_user_kthread`), and routes the child's `exit`
    // and the parent's `wait`/`exit` `ecall`s through the producer +
    // `reschedule_current`: the producer parks the parent until the child is
    // reapable (the RV1 mid-handler-park-safe path), then the kernel copies the
    // reaped exit code out to the parent's `status` pointer. PASS once the
    // parent reaped the child, read back the agreed code, and exited 0; a wrong
    // code, a missing reap, an unexpected syscall, or a stall writes a distinct
    // failure finisher or times out (fail-loud). Single CPU and
    // a 60-second budget match the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "rustos-test-wait-qemu-riscv64",
        binary: "rustos-test-wait-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // `plans/PI.md` P11 (root-volume read path at boot):
    // `rustos-test-users-db-qemu-aarch64` reuses the exact virtio-blk-mmio
    // bring-up above, then instead of a raw sector round-trip it mounts
    // the planted users-root rustfs volume through the real driver and
    // drives the kernel's boot-time users-database load
    // (`rustos_kernel_core::load_users_db`) — /System/Security/Users read
    // off the volume through the-checked VFS delegation — then
    // proves the parsed database authenticates the planted account and
    // refuses a wrong password before the ARM semihosting PASS. The
    // backing image is the fixture's users-root volume
    // (`FsDisk::UsersRoot`) — authored by the real rustfs driver — so its
    // geometry is the image's own size. Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-users-db-qemu-aarch64",
        binary: "rustos-test-users-db-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::UsersRoot,
        keyboard: None,
        serial: &[],
    },
    // `plans/PI.md` P11 Chunk B-2 (root-mount->login): the
    // `rustos-test-root-unlock-login-qemu-aarch64` vertical reuses the
    // exact virtio-blk-mmio bring-up above, then drives the *production*
    // interactive unlock policy
    // (`rustos_kernel::root_mount::unlock_root_disk_interactively`) over a
    // planted **whole-disk** encrypted-root image (`FsDisk::EncryptedRootDisk`
    // — MBR + FAT boot carrying `root.unlock` + a passphrase-derived
    // encrypted RustFS root): it reads the descriptor off the FAT boot
    // partition, types the fixture passphrase at the prompt over a scripted
    // console, mounts the encrypted root, installs the loaded users database
    // into a `LateUsersDb` cell, and proves the planted account authenticates
    // through the installed cell while a wrong password is refused — before
    // the ARM semihosting PASS. The backing image is the shared whole-disk
    // fixture's bytes — authored by the real in-tree drivers and split by the
    // `root_mount` host tests — so the planted layout and the guest's unlock
    // cannot drift. The root volume uses the format-floor
    // PBKDF2 cost so the per-boot key derivation stays bounded under QEMU TCG;
    // single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "rustos-test-root-unlock-login-qemu-aarch64",
        binary: "rustos-test-root-unlock-login-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        serial: &[],
    },
    // `plans/PI.md` P11 Chunk B-2 INCREMENT (2): the
    // `rustos-test-root-unlock-admission-qemu-aarch64` vertical boots the
    // *production* aarch64 `rustos-kernel` pipeline (`boot_aarch64::boot`)
    // on the `virt` board with the same planted whole-disk encrypted-root
    // image (`FsDisk::EncryptedRootDisk`) attached as a virtio-blk-mmio
    // device — but, unlike `root_unlock_login` (which drives the unlock
    // *policy* directly), it proves the *kthread admission* path: the
    // bootstrap-floor virtio-MMIO bus enumeration
    // (`root_storage::observe_virtio_mmio_block_devices`) probes the slot
    // and binds the virtio-blk root, the init seam admits the in-kernel
    // unlock kthread (`unlock_service::spawn_if_present`), and the kthread
    // brings the device up over the production device-IRQ path, prompts at
    // `Root passphrase: `, reads the typed passphrase, mounts the encrypted
    // `RustFS` root, and installs the users database into `LATE_USERS_DB`.
    // The kernel-side audit sink reports PASS through the ARM semihosting
    // finisher the instant it sees the unlock-service install message
    // (`EventId(4139)`) — the witness that the kthread-admission path
    // mounted the root end to end. The runner types only the fixture
    // passphrase (verified against the shared fixture at compile time,
    // `is_line_of`); the database *content* authenticating
    // `root`/`root` is proven by `root_unlock_login`, and the per-console
    // `login` authenticating end to end into a real shell session is the
    // session-ceiling vertical's job (below), so both are out of this
    // vertical's scope. A 90-second budget covers the boot + bounded PBKDF2
    // derivation on QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "rustos-test-root-unlock-admission-qemu-aarch64",
        binary: "rustos-test-root-unlock-admission-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(90),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        serial: &[("Root passphrase: ", UNLOCK_PASSPHRASE_LINE)],
    },
    // `plans/CAPABILITY_USE.md` CU3: the session-ceiling acceptance vertical.
    // `rustos-test-session-ceiling-qemu-aarch64` boots the *production*
    // aarch64 pipeline with the planted encrypted-root disk, unlocks the
    // root at the passphrase prompt, authenticates `root`/`root` at the
    // console login (the planted account's grant is the shared
    // administrator ceiling, `rustos_users::administrator_ceiling` — the
    // same set `tools/mkimage::debug_users_db` seeds a debug image with),
    // and drives the spawned shell through a real session: `cd` into the
    // account's home (`CAP_FS_ACCESS` — the B3 regression), `pwd` proving
    // the move, spawning `/Apps/Ps.app/Run` (`CAP_PROC_SPAWN`) and seeing
    // its process-list header, then the negative half — a `ulimit` bound
    // pair is *lowered* (ungated; both bounds, since the default soft bound
    // is unlimited and a soft bound may never exceed its hard bound) and
    // the hard bound is then *raised*: the raise needs
    // `CAP_RLIMIT_RAISE`, which the ceiling carries but the shell's
    // session-baseline manifest does not request, so the effective
    // `manifest ∩ ceiling` set lacks it and the kernel refuses the
    // `rlimit_set` with `PermissionDenied` (an administrator account never
    // widens a program past its own manifest). Each line is typed only
    // after its marker appeared (`pwd`'s output and the shell's denial
    // message are themselves markers), and the guest audit sink reports
    // PASS only once the audited `rlimit_set` rejection has been seen
    // *and* the scripted `exit` that follows it dispatches — so the denial
    // provably reached the transcript before the run ended. A 120-second
    // budget covers boot + bounded PBKDF2 + the multi-exchange dialogue on
    // QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "rustos-test-session-ceiling-qemu-aarch64",
        binary: "rustos-test-session-ceiling-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        serial: &[
            ("Root passphrase: ", UNLOCK_PASSPHRASE_LINE),
            ("Username: ", SESSION_USERNAME_LINE),
            ("Password: ", SESSION_PASSWORD_LINE),
            ("elsh$ ", "cd /Users/root\n"),
            ("elsh$ ", "pwd\n"),
            ("/Users/root", "/Apps/Ps.app/Run\n"),
            ("PID  PPID", "ulimit processes 1000\n"),
            ("elsh$ ", "ulimit -H processes 2000\n"),
            (
                "cannot raise hard limit (requires CAP_RLIMIT_RAISE)",
                "exit\n",
            ),
        ],
    },
    // `plans/PI.md` design B / B2: the pre-unlock driver-loading-by-discovery
    // autoload vertical. `rustos-test-autoload-input-qemu-aarch64` boots the
    // *production* aarch64 pipeline on the `virt` board with the
    // `FsDisk::AutoloadRootDisk` whole-disk image — whose read-only `/System`
    // volume carries a kernel-signed virtio-input keyboard driver bundle in its
    // `Drivers/` store — and an attached `virtio-keyboard-device`. The boot
    // binds the virtio-blk root and discovers the virtio-input node; the unlock
    // kthread mounts the read-only `/System` volume and its autoload hook scans
    // that volume's signed store **before** any passphrase prompt, verifies the
    // bundle against the kernel's embedded driver trust anchor, matches it to
    // the discovered virtio-input node, and spawns it into its own user-space
    // process (granted the node's resources plus the delegated
    // `CAP_INPUT_INJECT`). The runner then types the fixture passphrase to
    // unlock the encrypted root; once the unlock-service install message appears
    // (the keyboard driver was spawned earlier, pre-unlock), it injects a key
    // through the QEMU monitor; the autoloaded driver decodes it and delivers it
    // to the input-focus arbiter via `key_inject`. PASS the instant the
    // kernel-side audit sink sees the one-shot `AuditEvent::InputDelivered`
    // (`EventId(4050)`) — the witness that an autoloaded *user-space* input
    // driver is live and delivering input (design-B keyboard-up-before-unlock).
    // A 120-second budget covers the boot + bounded PBKDF2 + autoload + driver
    // bring-up + injection on QEMU TCG.
    QemuTest {
        package: "rustos-test-autoload-input-qemu-aarch64",
        binary: "rustos-test-autoload-input-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: Some((AUTOLOAD_INPUT_KEY_MARKER, "a")),
        serial: &[("Root passphrase: ", UNLOCK_PASSPHRASE_LINE)],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
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
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (stage G1):
    // `rustos-test-stack-guard-qemu-aarch64` proves the live mechanism the
    // kthread kernel-stack guard page is built on. It builds one stage-1
    // `paging::AddressSpace` (identity-maps the low 2 GiB), calls
    // `AddressSpace::split_block` to shatter the coarse identity block that
    // covers a dedicated `GUARD_PAGE` static down to 4 KiB pages
    // (preserving every mapping), installs the EL1 vectors + a `fault`
    // handler, enables the MMU, writes+reads-back a sentinel through the
    // guard page (proving the split preserved the mapping live), then
    // `unmap`s that single page through the Arch HAL + `flush_page`s its
    // stale TLB entry and reads it: the MMU raises a data abort, the
    // handler confirms the cause / faulting address, and reports PASS via
    // semihosting. A regression that fails to split, preserve, or unmap
    // either reports FAILURE explicitly or never faults (timing out).
    // Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-stack-guard-qemu-aarch64",
        binary: "rustos-test-stack-guard-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (stage G2):
    // `rustos-test-stack-arena-qemu-aarch64` proves the boot-time
    // kthread-stack guard arena (`AddressSpace::prepare_guard_arena`). It
    // builds one stage-1 `paging::AddressSpace` (identity-maps the low
    // 2 GiB), prepares a 2 MiB-aligned, 2 MiB guard arena at 4 KiB
    // granularity (the arena is its own L2 block, distinct from the block
    // holding the running code/stack), installs the EL1 vectors + a
    // `fault` handler, enables the MMU, writes+reads-back a sentinel
    // through an arena guard page (proving the split preserved the mapping
    // live), then `unmap`s that one page through the Arch HAL +
    // `flush_page`s it, proves the running stack (a different 2 MiB block)
    // and a neighbouring arena page still work, and reads the unmapped
    // page: the MMU raises a data abort, the handler confirms the cause /
    // faulting address, and reports PASS via semihosting. A regression
    // that shatters the running block, fails to preserve the arena, or
    // never faults either reports FAILURE explicitly or times out. Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "rustos-test-stack-arena-qemu-aarch64",
        binary: "rustos-test-stack-arena-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (stage G3c): the *production*
    // fault-form. `rustos-test-stack-overrun-qemu-aarch64` proves that an
    // overrunning kthread takes a synchronous data abort, not a
    // next-reschedule canary detection. It builds a stage-1 identity
    // `AddressSpace`, prepares a 2 MiB-aligned guard arena
    // (`AddressSpace::prepare_guard_arena`, G2), carves one kthread stack
    // region `[guard page | usable stack]` out of it, installs the EL1
    // vectors + a `fault` handler, enables the MMU, then `unmap`s the guard
    // page through the Arch HAL + `flush_page`s it — the production
    // guard-page mechanism (G3b-2). It then builds the live
    // `rustos-kernel-sched-eevdf` `Scheduler` over `Aarch64Arch`, admits a
    // kthread on that stack via `kernel_core::spawn_kthread_with_stack`, and
    // drives the cooperative `step` loop. The kthread body overruns its
    // stack (touches the highest guard byte, the first byte a contiguous
    // downward overrun crosses); because the guard page is unmapped the
    // access raises a synchronous data abort *while the kthread runs*, the
    // handler confirms the cause / faulting address, and reports PASS via
    // semihosting. A regression that left the page mapped lets the body
    // return cleanly; the drain loop then reports FAILURE explicitly rather
    // than passing. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "rustos-test-stack-overrun-qemu-aarch64",
        binary: "rustos-test-stack-overrun-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
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
        serial: &[],
    },
    // P11 Chunk B-2 INCREMENT (1) (`plans/PI.md`): the aarch64 device-SPI
    // -> parked-kthread vertical, the proof that the production aarch64
    // device-IRQ subsystem can wake an in-kernel service kthread (the
    // prerequisite for INCREMENT (2)'s root-unlock kthread). Where
    // `rustos-test-irq-qemu-aarch64` proves the device-IRQ *delivery* path
    // against a hard-coded INTID and a `wfi` poll loop, this vertical proves
    // the two INCREMENT (1) pieces that path serves: (1) DTB SPI discovery
    // (`fdt::gic_device_intid` decodes the PL031 RTC node's `interrupts`
    // triple into its GICv2 INTID from the embedded `virt` tree — no board
    // constant), and (2) the kthread-cooperative
    // `rustos_kernel_core::KthreadIrqWaiter`, driven by a real in-kernel
    // service kthread (`spawn_kthread`) through the shared
    // `block_until_ready` loop on the live `rustos-kernel-sched-eevdf`
    // `Scheduler`. The kthread parks on the bound RTC SPI, yielding each
    // cooperative `step`; when the RTC fires the EL1 GICv2 path masks the
    // line and sets the ready flag, the kthread observes `WaitOutcome::Ready`
    // and exits, and the kernel asserts the GIC line re-reads masked
    // (mask-before-wake, `docs/src/security/irq.md`) before the semihosting
    // PASS. A regression that fails to discover, deliver, wake, or mask never
    // reaches PASS, so the run times out (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "rustos-test-irq-kthread-qemu-aarch64",
        binary: "rustos-test-irq-kthread-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        disk_sectors: None,
        virtio_net: false,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        serial: &[],
    },
    // Stage W11-B (`plans/WIRING.md` §3): the aarch64 input vertical —
    // the `virt`-board virtio-input analogue of the x86_64 PS/2 vertical,
    // completing the `input` row of the QEMU matrix for aarch64.
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
        serial: &[],
    },
    // WIRING (`plans/WIRING.md` §1/§3): the riscv64 input vertical —
    // the `virt`-board virtio-input MMIO analogue of the aarch64 input
    // vertical, completing the `input` row of the QEMU matrix for
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
    // aarch64 vertical runs. Single CPU and a 60-second
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
        serial: &[],
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
/// re-runs the binaries rather than rebuilding them each pass ('s no-flaky-tests rule: the value of repetition is in the *runs*).
pub fn build_all(ctx: &Context) -> Result<(), String> {
    eprintln!("xtask: [test --qemu] {} test(s) enrolled", TESTS.len());
    // Group the enrolled packages by target triple and build each triple in a
    // single `cargo build`. One invocation per triple (rather than one per
    // enrolment) lets cargo compile that triple's packages concurrently and
    // share a single build-lock acquisition, instead of serialising behind the
    // lock once per test. The QEMU *runs* then execute concurrently under a
    // host-CPU budget — see [`run_once`] and `commands::parallel`.
    for target in build_targets() {
        let packages: Vec<&str> = TESTS
            .iter()
            .filter(|t| t.target == target)
            .map(|t| t.package)
            .collect();
        if packages.is_empty() {
            continue;
        }
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

/// Execute every enrolled QEMU test once, running guests concurrently.
///
/// The caller ([`super::run_test`]) owns the repeat loop so a duration
/// budget covers the whole matrix as a unit; this runs exactly one pass and
/// never retries on failure.
///
/// The enrolments are independent — each plants its own per-binary backing
/// images and drives a guest whose serial console is `-serial stdio` and
/// whose QEMU monitor is a unique per-run unix socket, so two guests share
/// no host resource except CPU. They are therefore run through the shared
/// weighted-concurrency runner ([`super::parallel`]): each guest's weight is
/// its emulated-CPU count and the budget is the host's logical-CPU count, so
/// the sum of concurrently-running guest vCPUs never oversubscribes the host.
/// That keeps every guest's wall-clock deadline as reachable as it is for a
/// solo run (no TCG starvation), so co-scheduling does not make a test flaky. On a single-core host the budget collapses to one and the matrix
/// runs strictly sequentially.
pub fn run_once(ctx: &Context) -> Result<(), String> {
    let target_dir = ctx.target_dir();
    let budget = parallel::host_parallelism();
    let jobs: Vec<Job> = TESTS
        .iter()
        .map(|t| {
            let label = format!("test --qemu (run {}) cpus={}", t.package, t.cpus);
            let weight = usize::try_from(t.cpus).unwrap_or(1);
            let target_dir = target_dir.clone();
            Job::closure(label, weight, move || run_one(&target_dir, t))
        })
        .collect();
    parallel::run(jobs, budget)
}

/// One enrolled QEMU integration test exposed for the long-CI flake hunt
/// ([`super::ci_long`]).
///
/// It carries only what the flake hunt needs — a human label, the
/// emulated-CPU weight the concurrency runner charges against its budget, and
/// a handle to the enrolment itself — so a single enrolment can be run
/// repeatedly without re-exposing the private [`QemuTest`] table. Copy so a
/// per-repetition job factory can capture it freely.
#[derive(Copy, Clone)]
pub(crate) struct Enrolment {
    /// Cargo package name, used to label the flake-hunt jobs.
    pub package: &'static str,
    /// Emulated-CPU count; the concurrency runner's per-job weight, so
    /// concurrent replicas of this test never oversubscribe the host.
    pub cpus: u32,
    /// The enrolment to drive; private so callers go through [`Self::run`].
    test: &'static QemuTest,
}

impl Enrolment {
    /// Drive this enrolment to completion once, exactly as [`run_once`] does,
    /// with no retry. `target_dir` is where the pre-built kernel binaries live
    /// (see [`build_all`]).
    pub(crate) fn run(&self, target_dir: &Path) -> Result<(), String> {
        run_one(target_dir, self.test)
    }
}

/// Every enrolled QEMU integration test, in registry order.
///
/// The single source of truth for the flake hunt's QEMU set is the same
/// `TESTS` table [`run_once`] drives, so the two can never diverge.
pub(crate) fn enrolments() -> Vec<Enrolment> {
    TESTS
        .iter()
        .map(|t| Enrolment {
            package: t.package,
            cpus: t.cpus,
            test: t,
        })
        .collect()
}

fn run_one(target_dir: &Path, t: &QemuTest) -> Result<(), String> {
    let kernel: PathBuf = target_dir.join(t.target).join("debug").join(t.binary);
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
    // One budget everywhere: the enrolment's own reachable wall-clock ceiling,
    // enforced identically on a developer machine and a CI runner. There is no
    // developer-only clamp — a budget that is reachable running solo but missed
    // under the parallel matrix would be a load-dependent (flaky) timeout, and
    // the charter forbids that. Concurrency, not the budget, is what bounds
    // local run time: the weighted-concurrency runner (`super::parallel`) caps
    // the sum of concurrently-running guest vCPUs at the host's logical-CPU
    // count, so no guest is starved of TCG time and every enrolled budget stays
    // as reachable co-scheduled as it is solo.
    let mut spec = base.with_cpus(t.cpus).with_timeout(t.timeout);

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
    // on-disk layout and the guest's expectations cannot drift: the FAT32 fixture is hand-built; the rustfs
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
        FsDisk::UsersRoot => Some((
            "users.img",
            rustos_test_rustfs_image::build_users_root_image().map_err(|e| {
                format!("test --qemu ({}): build users-root image: {e:?}", t.package)
            })?,
            rustos_test_rustfs_image::TOTAL_SECTORS,
        )),
        FsDisk::EncryptedRootDisk => Some((
            "encrypted-root.img",
            rustos_test_encrypted_root_image::build_image().map_err(|e| {
                format!(
                    "test --qemu ({}): build encrypted-root image: {e:?}",
                    t.package
                )
            })?,
            rustos_test_encrypted_root_image::TOTAL_SECTORS,
        )),
        FsDisk::AutoloadRootDisk => Some((
            "autoload-root.img",
            rustos_test_autoload_root_image::build_image().map_err(|e| {
                format!(
                    "test --qemu ({}): build autoload-root image: {e:?}",
                    t.package
                )
            })?,
            rustos_test_autoload_root_image::TOTAL_SECTORS,
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

    // Pipe QEMU's stdin for the interactive-session vertical and let the
    // runner replay the scripted exchange, each line typed once the guest
    // prints that step's prompt.
    for (marker, line) in t.serial {
        spec = spec.with_serial_input(*marker, *line);
    }

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
    use super::{build_targets, TESTS};
    use std::time::Duration;

    /// The smallest wall-clock budget any enrolment may carry.
    ///
    /// Every enrolled QEMU test is a boot-then-do-fixed-work vertical whose
    /// budget is sized to be reachable when the guest runs co-scheduled with
    /// the rest of the matrix (the weighted-concurrency runner never
    /// oversubscribes the host's vCPUs), not merely when it runs solo. This
    /// floor is the reachable minimum the guard below enforces; the runner
    /// applies each enrolment's own [`super::QemuTest::timeout`] verbatim on
    /// both a developer machine and a CI runner, with no split that could
    /// shorten it.
    const MIN_REACHABLE_BUDGET: Duration = Duration::from_secs(60);

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

    /// Regression guard for the removed developer-only timeout clamp. Every
    /// enrolment must carry a budget at least [`MIN_REACHABLE_BUDGET`], and
    /// that budget is what the runner enforces verbatim — there is no
    /// developer-vs-CI split that could shorten it. A previous 30 s
    /// developer cap halved these budgets locally and turned a guest that was
    /// merely slow under the parallel matrix into a load-dependent (flaky)
    /// timeout; nothing may re-introduce a budget, or a clamp, below this
    /// floor.
    #[test]
    fn every_enrolment_budget_is_at_least_the_reachable_floor() {
        for t in TESTS {
            assert!(
                t.timeout >= MIN_REACHABLE_BUDGET,
                "enrolment {} budget {:?} is below the reachable floor {:?}; a \
                 budget reachable solo but missed under load is a flaky timeout",
                t.package,
                t.timeout,
                MIN_REACHABLE_BUDGET,
            );
        }
    }
}
