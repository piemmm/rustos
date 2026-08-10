//! aarch64-specific QEMU defaults and argv assembly (Stage 3b).
//!
//! The generic [`crate::Spec`] is architecture-neutral; everything that
//! is *only* meaningful when targeting `qemu-system-aarch64` on the
//! generic `virt` board lives here:
//!
//! * the default guest RAM size and CPU model,
//! * the ARM semihosting result protocol the kernel reports through,
//! * the exact QEMU argv the runner emits,
//! * decoding the QEMU process exit status under that protocol.
//!
//! Splitting this surface out mirrors [`crate::riscv64`] and
//! [`crate::x86_64`] and keeps [`crate::Spec`] honest as a per-arch
//! tagged union without duplicating any glue (no duplication, no interface creep).
//!
//! # Boot model
//!
//! `qemu-system-aarch64 -M virt -kernel <elf>` loads the ELF at its link
//! address and enters its entry point with the Linux aarch64 boot
//! protocol hand-off (`x0 = DTB`). Like every other port,
//! [`crate::Runner::run`] hands `spec.kernel` straight to this module's
//! argv builder — no boot media is built.
//!
//! # Result protocol
//!
//! The kernel reports its result through ARM semihosting (`SYS_EXIT`):
//! a success exit makes QEMU exit with status `0`
//! ([`SUCCESS_EXIT_STATUS`]); any other status is a failure. This
//! matches riscv64's zero-is-pass convention and is the inverse of
//! x86_64's `isa-debug-exit` (where success is a *non-zero* status), so
//! the decode is per-arch — see [`outcome_from_status`].

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::{net_device_arg, netdev_dgram_arg, Outcome, SessionKind, Spec};

/// Default guest RAM size in mebibytes for an aarch64 QEMU integration
/// test. Matches the x86_64 and riscv64 defaults so all three ports
/// share one mental model.
pub const DEFAULT_RAM_MIB: u32 = 256;

/// Name of the `qemu-system-*` binary for aarch64.
pub const QEMU_BINARY: &str = "qemu-system-aarch64";

/// QEMU machine model the runner targets — the generic `virt` board,
/// the only aarch64 platform TAIRiX' QEMU tests run on. It carries a
/// PL011 UART, a GICv2, the ARM generic timer, and a virtio-mmio bus.
pub const MACHINE: &str = "virt";

/// CPU model. `cortex-a72` is a widely-available ARMv8-A core (the
/// Raspberry Pi 4's CPU) that QEMU's `virt` board models cleanly under
/// TCG.
pub const CPU: &str = "cortex-a72";

/// QEMU host-process exit status the semihosting `SYS_EXIT` success path
/// produces. Like riscv64's `SiFive` Test finisher, success is a plain
/// zero exit status (unlike x86_64's `isa-debug-exit`).
pub const SUCCESS_EXIT_STATUS: i32 = 0;

/// Decode a QEMU exit status under the ARM semihosting `SYS_EXIT`
/// protocol.
///
/// Returns [`Outcome::Pass`] iff `status == SUCCESS_EXIT_STATUS` (`0`).
/// Every other status is treated as [`Outcome::Fail`]. Both carry the
/// captured serial log.
#[must_use]
pub fn outcome_from_status(status: i32, serial: String) -> Outcome {
    if status == SUCCESS_EXIT_STATUS {
        Outcome::Pass { serial }
    } else {
        Outcome::Fail { status, serial }
    }
}

/// Push the aarch64 QEMU argv onto `cmd`.
///
/// Emits the canonical `virt`-board invocation: `-M virt`,
/// `-cpu cortex-a72`, headless display, serial over stdio,
/// `-m {DEFAULT_RAM_MIB}M`, `-smp {spec.cpus}`, `-no-reboot`,
/// `-semihosting-config enable=on,target=native` (the test-result
/// channel), the kernel ELF via `-kernel`, and each backing image as a
/// `virtio-blk-device` / network interface as a `virtio-net-device` on
/// the board's virtio-mmio bus.
pub(crate) fn push_argv(cmd: &mut Command, spec: &Spec, kernel: &Path) {
    for arg in build_argv(spec, kernel) {
        cmd.arg(arg);
    }
}

/// Pure argv builder used by [`push_argv`] and the host unit tests.
///
/// Splitting the pure builder out keeps the argv-assembly contract
/// unit-testable without spawning QEMU.
fn build_argv(spec: &Spec, kernel: &Path) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::with_capacity(18 + spec.extra_args.len() * 2);
    argv.push("-M".into());
    argv.push(MACHINE.into());
    argv.push("-cpu".into());
    argv.push(CPU.into());
    argv.push("-no-reboot".into());
    // Headless by default (the test runner captures serial only); an
    // interactive run instead presents QEMU's default windowed display
    // backend so a human sees the guest's ramfb scan-out.
    if spec.session == SessionKind::HeadlessTest {
        argv.push("-display".into());
        argv.push("none".into());
    }
    argv.push("-serial".into());
    argv.push("stdio".into());
    // ARM semihosting is how the freestanding kernel reports PASS/FAIL.
    // `target=native` routes the call to QEMU itself (not a debugger).
    argv.push("-semihosting-config".into());
    argv.push("enable=on,target=native".into());
    argv.push("-m".into());
    argv.push(format!("{DEFAULT_RAM_MIB}M").into());
    argv.push("-smp".into());
    argv.push(spec.cpus.to_string().into());
    // Attach QEMU's `ramfb` display device when requested (firmware-
    // programmed linear framebuffer in guest RAM, programmed over
    // `fw_cfg`), the display-class analogue of the virtio-mmio devices.
    if spec.display_ramfb {
        argv.push("-device".into());
        argv.push("ramfb".into());
    }
    // Present every virtio-mmio transport as a *modern* (virtio 1.x,
    // version 2) device, matching the riscv64 `virt` board policy.
    argv.push("-global".into());
    argv.push("virtio-mmio.force-legacy=false".into());
    argv.push("-kernel".into());
    argv.push(kernel.into());

    // Attach each backing image as a virtio-mmio block device.
    for (i, dev) in spec.block_devices.iter().enumerate() {
        argv.push("-drive".into());
        let mut drive = OsString::from(format!("if=none,format=raw,id=blk{i},file="));
        drive.push(dev.image.as_os_str());
        argv.push(drive);
        argv.push("-device".into());
        argv.push(format!("virtio-blk-device,drive=blk{i}").into());
    }

    // Suppress QEMU's implicit default NIC for a vertical that attaches no
    // network device of its own. Without this the `virt` board auto-creates
    // a default virtio-net device whenever no networking option is given,
    // which the guest's bootstrap-floor discovery then enumerates and
    // autoloads a driver for — a phantom interface no vertical asked for,
    // whose driver/stack churn starves an otherwise network-free guest. A
    // vertical that *does* attach an explicit `-netdev` below already
    // overrides the default, so `-net none` is added only when there is none.
    if spec.net_devices.is_empty() {
        argv.push("-net".into());
        argv.push("none".into());
    }

    // Attach each network interface as a virtio-mmio net device behind a
    // `dgram` unix-datagram backend (the harness is the guest's link
    // peer), with an optional pcap mirror.
    for (i, dev) in spec.net_devices.iter().enumerate() {
        argv.push("-netdev".into());
        argv.push(netdev_dgram_arg(i, dev));
        argv.push("-device".into());
        argv.push(net_device_arg("virtio-net-device", i, dev, ""));
        if let Some(pcap) = &dev.pcap {
            argv.push("-object".into());
            let mut filter = OsString::from(format!("filter-dump,id=dump{i},netdev=net{i},file="));
            filter.push(pcap.as_os_str());
            argv.push(filter);
        }
    }

    // Attach a virtio-mmio keyboard for the input vertical (the runner
    // drives the scripted key or typed text through the QEMU monitor once
    // the guest signals readiness) or for a human typing into the
    // interactive window; here we only present the device. The interactive
    // session also gets a virtio-mmio mouse for pointer input from the
    // window.
    let interactive = spec.session == SessionKind::WindowedInteractive;
    if spec.input_keyboard.is_some() || !spec.input_typing.is_empty() || interactive {
        argv.push("-device".into());
        argv.push("virtio-keyboard-device".into());
    }
    if interactive || spec.input_mouse {
        argv.push("-device".into());
        argv.push("virtio-mouse-device".into());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arch;
    use std::path::PathBuf;
    use std::time::Duration;

    fn fixture_spec(cpus: u32) -> Spec {
        Spec {
            arch: Arch::Aarch64,
            kernel: PathBuf::from("/tmp/k.elf"),
            cpus,
            timeout: Duration::from_secs(60),
            declared_runtime_ceiling: None,
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            display_ramfb: false,
            extra_args: Vec::new(),
            input_keyboard: None,
            input_typing: Vec::new(),
            input_mouse: false,
            pointer_script: Vec::new(),
            serial_input: Vec::new(),
            screendumps: Vec::new(),
            monitor_commands: Vec::new(),
            session: SessionKind::HeadlessTest,
            reset_success_marker: None,
            completion_gate: None,
        }
    }

    fn render(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn qemu_binary_name_is_arch_specific() {
        assert_eq!(QEMU_BINARY, "qemu-system-aarch64");
    }

    #[test]
    fn default_ram_matches_the_other_ports_for_one_mental_model() {
        assert_eq!(DEFAULT_RAM_MIB, crate::riscv64::DEFAULT_RAM_MIB);
    }

    #[test]
    fn pass_is_zero_exit_status() {
        assert_eq!(SUCCESS_EXIT_STATUS, 0);
        assert!(matches!(
            outcome_from_status(0, String::new()),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn non_zero_exit_status_is_fail_with_serial() {
        match outcome_from_status(7, "log".into()) {
            Outcome::Fail { status, serial } => {
                assert_eq!(status, 7);
                assert_eq!(serial, "log");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn windowed_interactive_argv_shows_a_display_and_attaches_input_devices() {
        let mut spec = fixture_spec(1);
        spec.session = SessionKind::WindowedInteractive;
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(
            !argv.iter().any(|a| a == "-display"),
            "windowed run leaves QEMU's default display backend in place"
        );
        assert!(argv.iter().any(|a| a == "virtio-keyboard-device"));
        assert!(argv.iter().any(|a| a == "virtio-mouse-device"));
    }

    #[test]
    fn headless_default_argv_attaches_no_human_input_devices() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(!argv.iter().any(|a| a == "virtio-keyboard-device"));
        assert!(!argv.iter().any(|a| a == "virtio-mouse-device"));
    }

    #[test]
    fn argv_selects_the_virt_machine_and_cpu() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let m = argv
            .iter()
            .position(|a| a == "-M")
            .expect("argv contains -M");
        assert_eq!(argv[m + 1], MACHINE);
        let c = argv
            .iter()
            .position(|a| a == "-cpu")
            .expect("argv contains -cpu");
        assert_eq!(argv[c + 1], CPU);
    }

    #[test]
    fn argv_enables_semihosting_for_the_result_channel() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let pos = argv
            .iter()
            .position(|a| a == "-semihosting-config")
            .expect("argv enables semihosting");
        assert_eq!(argv[pos + 1], "enable=on,target=native");
    }

    #[test]
    fn argv_boots_the_kernel_elf_without_a_bios() {
        let spec = fixture_spec(1);
        let kernel = Path::new("/tmp/mykernel.elf");
        let argv = render(&build_argv(&spec, kernel));
        assert!(
            !argv.iter().any(|a| a == "-bios"),
            "virt -kernel needs no -bios"
        );
        let kpos = argv
            .iter()
            .position(|a| a == "-kernel")
            .expect("argv contains -kernel");
        assert_eq!(argv[kpos + 1], kernel.to_string_lossy().into_owned());
    }

    #[test]
    fn argv_contains_documented_invariant_flags() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(argv.iter().any(|a| a == "-no-reboot"));
        assert!(argv.iter().any(|a| a == "-display"));
        assert!(argv.iter().any(|a| a == "none"));
        assert!(argv.iter().any(|a| a == "-serial"));
        assert!(argv.iter().any(|a| a == "stdio"));
    }

    #[test]
    fn argv_encodes_ram_and_cpu_count() {
        let spec = fixture_spec(4);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let m = argv
            .iter()
            .position(|a| a == "-m")
            .expect("argv contains -m");
        assert_eq!(argv[m + 1], format!("{DEFAULT_RAM_MIB}M"));
        let smp = argv
            .iter()
            .position(|a| a == "-smp")
            .expect("argv contains -smp");
        assert_eq!(argv[smp + 1], "4");
    }

    #[test]
    fn argv_forces_modern_virtio_mmio() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let pos = argv
            .iter()
            .position(|a| a == "-global")
            .expect("argv contains -global");
        assert_eq!(argv[pos + 1], "virtio-mmio.force-legacy=false");
    }

    #[test]
    fn argv_attaches_block_and_net_devices_as_virtio_mmio() {
        let mut spec = fixture_spec(1);
        spec.block_devices = vec![crate::BlockDevice {
            image: PathBuf::from("/tmp/disk0.img"),
        }];
        spec.net_devices = vec![crate::NetDevice {
            qemu_sock: PathBuf::from("/tmp/net0.qemu.sock"),
            peer_sock: PathBuf::from("/tmp/net0.peer.sock"),
            pcap: Some(PathBuf::from("/tmp/cap0.pcap")),
            mac: None,
        }];
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(argv.iter().any(|a| a == "virtio-blk-device,drive=blk0"));
        assert!(argv.iter().any(|a| a
            == "dgram,id=net0,local.type=unix,local.path=/tmp/net0.qemu.sock,\
                remote.type=unix,remote.path=/tmp/net0.peer.sock"));
        assert!(argv.iter().any(|a| a == "virtio-net-device,netdev=net0"));
        assert!(argv
            .iter()
            .any(|a| a.contains("filter-dump") && a.contains("/tmp/cap0.pcap")));
    }

    #[test]
    fn argv_without_devices_attaches_none() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(!argv.iter().any(|a| a.starts_with("virtio-blk-device")));
        assert!(!argv.iter().any(|a| a.starts_with("virtio-net-device")));
        assert!(!argv.iter().any(|a| a == "ramfb"));
        assert!(!argv.iter().any(|a| a == "virtio-keyboard-device"));
    }

    #[test]
    fn argv_attaches_keyboard_when_input_requested() {
        let mut spec = fixture_spec(1);
        spec.input_keyboard = Some(crate::KeyInjection {
            ready_marker: "ready".into(),
            key: "a".into(),
            ready_occurrences: 1,
        });
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(argv.iter().any(|a| a == "virtio-keyboard-device"));
    }

    #[test]
    fn argv_attaches_keyboard_when_typing_requested() {
        // A typed-text script needs the same virtio keyboard the single-key
        // injection attaches, even with no `input_keyboard` request.
        let mut spec = fixture_spec(1);
        spec.input_typing = vec![crate::KeyTyping {
            ready_marker: "armed".into(),
            ready_occurrences: 2,
            text: "hunter2\n".into(),
        }];
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(argv.iter().any(|a| a == "virtio-keyboard-device"));
    }

    #[test]
    fn argv_attaches_mouse_after_keyboard_when_pointer_requested() {
        // The two-identical-virtio-input-nodes topology an interactive
        // session presents: the pointer sibling rides after the keyboard
        // on the command line, so a headless vertical reproduces the
        // exact enumeration order a human-facing run gets.
        let mut spec = fixture_spec(1);
        spec.input_keyboard = Some(crate::KeyInjection {
            ready_marker: "ready".into(),
            key: "a".into(),
            ready_occurrences: 1,
        });
        spec.input_mouse = true;
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let kbd = argv
            .iter()
            .position(|a| a == "virtio-keyboard-device")
            .expect("keyboard attached");
        let mouse = argv
            .iter()
            .position(|a| a == "virtio-mouse-device")
            .expect("mouse attached");
        assert!(kbd < mouse, "mouse rides after the keyboard");
    }
}
