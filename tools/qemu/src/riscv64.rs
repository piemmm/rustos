//! riscv64-specific QEMU defaults and argv assembly (Stage 4.D Item 4).
//!
//! The generic [`crate::Spec`] is architecture-neutral; everything that
//! is *only* meaningful when targeting `qemu-system-riscv64` on the
//! generic `virt` board lives here:
//!
//! * the default guest RAM size,
//! * the `SiFive` Test (`sifive_test`) finisher constants the kernel
//!   writes to report its result,
//! * the exact QEMU argv the runner emits,
//! * decoding the QEMU process exit status under the finisher protocol.
//!
//! Splitting this surface out mirrors [`crate::x86_64`] and keeps
//! [`crate::Spec`] honest as a per-arch tagged union without duplicating
//! any glue (no duplication, no interface
//! creep).
//!
//! # Boot model
//!
//! `-bios default` loads the `OpenSBI` firmware bundled with QEMU, which
//! then jumps to the ELF supplied via `-kernel`. Like every other port,
//! [`crate::Runner::run`] hands `spec.kernel` straight to this module's
//! argv builder, which passes it to `-kernel`.
//!
//! # Result protocol
//!
//! The `virt` board exposes a `SiFive` Test device. The kernel reports its
//! result by writing a 32-bit word to [`SIFIVE_TEST_BASE`]:
//!
//! * [`FINISHER_PASS`] (`0x5555`) makes QEMU exit with process status
//!   `0`. The runner treats this — and **only** this — as success.
//! * [`FINISHER_FAIL`] (`0x3333`) in the low half, with an exit code in
//!   the high half (`(code << 16) | FINISHER_FAIL`), makes QEMU exit
//!   with process status `code`. Every non-zero status is a failure.
//!
//! This differs from x86_64's `isa-debug-exit` convention (where success
//! is a *non-zero* status), so the exit-status decode is per-arch — see
//! [`outcome_from_status`].
//!
//! # No `unwrap` / `expect` / `panic!`
//!
//! The `virt` argv assembly is infallible, so this module has no
//! production `Result` to propagate. The only `expect`s
//! in this file live inside `#[cfg(test)]` blocks 's tests
//! carve-out.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::{net_device_arg, netdev_dgram_arg, Outcome, SessionKind, Spec};

/// Default guest RAM size in mebibytes for a riscv64 QEMU integration
/// test.
///
/// 256 MiB matches the x86_64 default ([`crate::x86_64::DEFAULT_RAM_MIB`])
/// so both ports share one mental model; the `virt` board's own minimum
/// is far smaller, but keeping the two ports aligned removes a needless
/// per-arch variable. Callers cannot override this today; a future test
/// that needs more RAM should add a `with_ram_mib(n)` builder on
/// [`Spec`] rather than smuggling it through `extra_args`.
pub const DEFAULT_RAM_MIB: u32 = 256;

/// Name of the `qemu-system-*` binary for riscv64.
pub const QEMU_BINARY: &str = "qemu-system-riscv64";

/// QEMU machine model the runner targets. The generic `virt` board is
/// the only riscv64 platform TAIRiX' QEMU tests run on — it carries the
/// `SiFive` Test device, eight virtio-mmio transports, and a `PCIe` host
/// bridge the Stage 4.D drivers exercise.
pub const MACHINE: &str = "virt";

/// CPU model the runner targets: the generic RV64 core with
/// **software-managed** page-table Accessed/Dirty bits
/// (`svade=true,svadu=false`). RISC-V leaves A/D update
/// implementation-defined; forcing the Svade (fault-on-update) behaviour
/// makes the `virt` board exercise the kernel's software A/D-setting fault
/// path (`kernel/arch/riscv64::paging::set_accessed_flag_in_active`) that
/// silicon without hardware A/D update requires — the path the cold-page
/// referenced bit (`plans/SWAPSWAPSWAP.md`) depends on. TAIRiX maps every
/// leaf with A/D already set, so ordinary operation never faults under
/// either behaviour; this is the conservative, portable choice.
pub const CPU: &str = "rv64,svade=true,svadu=false";

/// MMIO base address of the `virt` board's `SiFive` Test device. The
/// kernel writes a finisher word here to report its result; the value
/// is fixed by the QEMU `virt` memory map and mirrored by the kernel
/// side (`kernel/arch/riscv64::qemu_exit`).
pub const SIFIVE_TEST_BASE: u64 = 0x10_0000;

/// `SiFive` Test finisher word the kernel writes to report success. QEMU
/// exits the host process with status `0` in response.
pub const FINISHER_PASS: u32 = 0x5555;

/// `SiFive` Test finisher word the kernel writes to report failure. The
/// kernel ORs an exit code into the high 16 bits
/// (`(code << 16) | FINISHER_FAIL`); QEMU exits the host process with
/// that `code`.
pub const FINISHER_FAIL: u32 = 0x3333;

/// QEMU host-process exit status that the `SiFive` Test [`FINISHER_PASS`]
/// write produces. Unlike x86_64's `isa-debug-exit`, the riscv64
/// finisher reports success as a plain zero exit status.
pub const SUCCESS_EXIT_STATUS: i32 = 0;

/// Decode a QEMU exit status under the `SiFive` Test finisher protocol.
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

/// Push the riscv64 QEMU argv onto `cmd`.
///
/// Emits the canonical `virt`-board invocation: `-M virt`, headless
/// display, serial over stdio, `-m {DEFAULT_RAM_MIB}M`,
/// `-smp {spec.cpus}`, `-no-reboot`, `-bios default` (`OpenSBI`), the
/// kernel ELF via `-kernel`, and each backing image as a
/// `virtio-blk-device` on the board's virtio-mmio bus.
///
/// Unlike the x86_64 backend there is no fallible boot-artifact build
/// step: the `virt` board boots the ELF directly through `OpenSBI`
/// (`-bios default` + `-kernel`), so [`crate::Runner::run`] passes
/// `spec.kernel` straight through and this builder is infallible.
pub(crate) fn push_argv(cmd: &mut Command, spec: &Spec, kernel: &Path) {
    for arg in build_argv(spec, kernel) {
        cmd.arg(arg);
    }
}

/// Pure argv builder used by [`push_argv`] and the host unit tests.
///
/// Splitting the pure builder out keeps the argv-assembly contract
/// unit-testable without spawning QEMU. The list is intentionally
/// returned as `Vec<OsString>` so callers can inspect it before
/// spawning QEMU.
fn build_argv(spec: &Spec, kernel: &Path) -> Vec<OsString> {
    // `-display none` + explicit `-serial stdio` gives headless boot
    // without the implicit stdio muxing `-nographic` would impose — the
    // same rationale documented on the x86_64 builder.
    let mut argv: Vec<OsString> = Vec::with_capacity(18 + spec.extra_args.len() * 2);
    argv.push("-M".into());
    argv.push(MACHINE.into());
    // Pin the CPU to `rv64` with **software-managed** page-table A/D bits
    // (`svade=true,svadu=false`). RISC-V leaves A/D update
    // implementation-defined: QEMU's default `virt` CPU updates them in
    // the walk (Svadu), but conservative silicon raises a page fault when
    // the Accessed/Dirty bit must be set, leaving software to set it
    // (Svade). TAIRiX maps every leaf with A/D already set, so ordinary
    // operation never faults under either behaviour; forcing Svade makes
    // the board exercise the *software* A/D-setting fault path the kernel
    // must carry for such silicon (`kernel/arch/riscv64::paging::
    // set_accessed_flag_in_active`, `plans/SWAPSWAPSWAP.md`), which the
    // cold-page referenced bit depends on — otherwise the path would be
    // dead under the default Svadu CPU. This is the conservative, portable
    // choice and is what the `accessed-bit-qemu-riscv64` vertical relies on.
    argv.push("-cpu".into());
    argv.push(CPU.into());
    argv.push("-no-reboot".into());
    // Headless by default (the test runner captures serial only); an
    // interactive windowed run omits `-display` here so the runner can append
    // the windowing backend it selected at spawn time.
    if spec.session == SessionKind::HeadlessTest {
        argv.push("-display".into());
        argv.push("none".into());
    }
    argv.push("-serial".into());
    argv.push("stdio".into());
    argv.push("-m".into());
    argv.push(format!("{DEFAULT_RAM_MIB}M").into());
    argv.push("-smp".into());
    argv.push(spec.cpus.to_string().into());
    argv.push("-bios".into());
    argv.push("default".into());
    // Attach QEMU's `ramfb` display device when requested. `ramfb` is a
    // firmware-programmed linear framebuffer whose scan-out surface
    // lives in guest RAM; the guest programs its geometry over the
    // `fw_cfg` device the `virt` board already carries. This is what the
    // framebuffer-display vertical drives.
    if spec.display_ramfb {
        argv.push("-device".into());
        argv.push("ramfb".into());
    }
    // Present every virtio-mmio transport as a *modern* (virtio 1.x,
    // version 2) device. QEMU's virtio-mmio defaults to the legacy
    // (version 1) interface for backwards compatibility; TAIRiX' MMIO
    // transport only drives the modern layout, so force it board-wide.
    argv.push("-global".into());
    argv.push("virtio-mmio.force-legacy=false".into());
    argv.push("-kernel".into());
    argv.push(kernel.into());

    // Attach each backing image as a virtio-mmio block device. `if=none`
    // detaches the drive from any automatic controller so the explicit
    // `-device virtio-blk-device,drive=blkN` is the only thing that
    // surfaces it to the guest — that device binds to one of the `virt`
    // board's virtio-mmio transports, which the Stage 4.D `MmioTransport`
    // drives (the riscv64 analogue of the x86_64 `virtio-blk-pci` path).
    for (i, dev) in spec.block_devices.iter().enumerate() {
        argv.push("-drive".into());
        let mut drive = OsString::from(format!("if=none,format=raw,id=blk{i},file="));
        drive.push(dev.image.as_os_str());
        argv.push(drive);
        argv.push("-device".into());
        argv.push(format!("virtio-blk-device,drive=blk{i}").into());
    }

    // Suppress QEMU's implicit default NIC for a network-free vertical (see
    // the aarch64 builder): the `virt` board auto-creates a default
    // virtio-net device when no networking option is given, a phantom
    // interface a guest's discovery would enumerate. Added only when no
    // explicit `-netdev` is attached below (which already overrides it).
    if spec.net_devices.is_empty() {
        argv.push("-net".into());
        argv.push("none".into());
    }

    // Attach each network interface as a virtio-mmio net device behind a
    // `dgram` unix-datagram backend — the riscv64 analogue of the x86_64
    // `virtio-net-pci` path. `virtio-net-device` binds to one of the
    // `virt` board's virtio-mmio transports, which the Stage 4.D
    // `MmioTransport` drives; the dgram socket pair makes the harness
    // the guest's link peer on a private per-run wire. An optional
    // `filter-dump` mirrors every frame to a host pcap.
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

    // Attach a virtio-mmio keyboard for the input vertical. The runner
    // (not this builder) drives the actual key through the QEMU monitor
    // once the guest signals readiness; here we only present the device.
    if spec.input_keyboard.is_some() {
        argv.push("-device".into());
        argv.push("virtio-keyboard-device".into());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, SessionKind};
    use std::path::PathBuf;
    use std::time::Duration;

    fn fixture_spec(cpus: u32) -> Spec {
        Spec {
            arch: Arch::Riscv64,
            kernel: PathBuf::from("/tmp/k.elf"),
            cpus,
            timeout: Duration::from_secs(60),
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
        assert_eq!(QEMU_BINARY, "qemu-system-riscv64");
    }

    #[test]
    fn default_ram_matches_x86_64_for_one_mental_model() {
        assert_eq!(DEFAULT_RAM_MIB, crate::x86_64::DEFAULT_RAM_MIB);
    }

    #[test]
    fn finisher_constants_match_sifive_test_documentation() {
        // Pinned by the QEMU `hw/misc/sifive_test.c` device model; the
        // kernel side (`kernel/arch/riscv64::qemu_exit`) writes the same
        // words to `SIFIVE_TEST_BASE`. A drift here is a silent
        // test-protocol break.
        assert_eq!(FINISHER_PASS, 0x5555);
        assert_eq!(FINISHER_FAIL, 0x3333);
        assert_eq!(SIFIVE_TEST_BASE, 0x10_0000);
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
        match outcome_from_status(42, "log".into()) {
            Outcome::Fail { status, serial } => {
                assert_eq!(status, 42);
                assert_eq!(serial, "log");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn argv_selects_the_virt_machine() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let pos = argv
            .iter()
            .position(|a| a == "-M")
            .expect("argv contains -M");
        assert_eq!(argv[pos + 1], MACHINE);
    }

    #[test]
    fn argv_forces_software_ad_cpu() {
        // The riscv64 runner pins the CPU to software-managed A/D
        // (Svade) so the kernel's software A/D-setting fault path is
        // genuinely exercised (`plans/SWAPSWAPSWAP.md`), rather than
        // shadowed by QEMU's default hardware A/D update.
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let pos = argv
            .iter()
            .position(|a| a == "-cpu")
            .expect("argv contains -cpu");
        assert_eq!(argv[pos + 1], CPU);
        assert!(CPU.contains("svade=true"), "A/D updates must trap");
        assert!(CPU.contains("svadu=false"), "hardware A/D must be off");
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
    fn windowed_interactive_argv_omits_display_for_the_runner_to_choose() {
        // The interactive display backend is selected and appended by the
        // runner at spawn time; the builder must not pin `-display none`, or
        // the window would never open.
        let mut spec = fixture_spec(1);
        spec.session = SessionKind::WindowedInteractive;
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(
            !argv.iter().any(|a| a == "-display"),
            "windowed run leaves the display for the runner to select"
        );
    }

    #[test]
    fn argv_boots_opensbi_then_the_kernel_elf() {
        let spec = fixture_spec(1);
        let kernel = Path::new("/tmp/mykernel.elf");
        let argv = render(&build_argv(&spec, kernel));
        let bios = argv
            .iter()
            .position(|a| a == "-bios")
            .expect("argv contains -bios");
        assert_eq!(argv[bios + 1], "default");
        let kpos = argv
            .iter()
            .position(|a| a == "-kernel")
            .expect("argv contains -kernel");
        assert_eq!(argv[kpos + 1], kernel.to_string_lossy().into_owned());
    }

    #[test]
    fn argv_forces_modern_virtio_mmio() {
        // TAIRiX' MMIO transport only drives modern (version 2)
        // virtio-mmio; the runner must override QEMU's legacy default.
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let pos = argv
            .iter()
            .position(|a| a == "-global")
            .expect("argv contains -global");
        assert_eq!(argv[pos + 1], "virtio-mmio.force-legacy=false");
    }

    #[test]
    fn argv_encodes_ram_size_in_mebibytes() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let mem_pos = argv
            .iter()
            .position(|a| a == "-m")
            .expect("argv contains -m");
        assert_eq!(argv[mem_pos + 1], format!("{DEFAULT_RAM_MIB}M"));
    }

    #[test]
    fn argv_encodes_cpu_count() {
        for n in [1u32, 4, 8] {
            let spec = fixture_spec(n);
            let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
            let pos = argv
                .iter()
                .position(|a| a == "-smp")
                .expect("argv contains -smp");
            assert_eq!(argv[pos + 1], n.to_string());
        }
    }

    #[test]
    fn argv_without_block_devices_attaches_no_virtio_blk() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(
            !argv.iter().any(|a| a.starts_with("virtio-blk-device")),
            "a storage-free spec must not attach a virtio-blk device"
        );
    }

    #[test]
    fn argv_attaches_each_block_device_as_virtio_blk_mmio() {
        let mut spec = fixture_spec(1);
        spec.block_devices = vec![
            crate::BlockDevice {
                image: PathBuf::from("/tmp/disk0.img"),
            },
            crate::BlockDevice {
                image: PathBuf::from("/tmp/disk1.img"),
            },
        ];
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));

        assert!(argv.iter().any(|a| a.contains("if=none")
            && a.contains("id=blk0")
            && a.contains("/tmp/disk0.img")));
        assert!(argv.iter().any(|a| a.contains("if=none")
            && a.contains("id=blk1")
            && a.contains("/tmp/disk1.img")));
        assert!(argv.iter().any(|a| a == "virtio-blk-device,drive=blk0"));
        assert!(argv.iter().any(|a| a == "virtio-blk-device,drive=blk1"));
    }

    #[test]
    fn argv_without_ramfb_attaches_no_display_device() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(
            !argv.iter().any(|a| a == "ramfb"),
            "a display-free spec must not attach a ramfb device"
        );
    }

    #[test]
    fn argv_attaches_ramfb_when_requested() {
        let mut spec = fixture_spec(1);
        spec.display_ramfb = true;
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        let pos = argv
            .iter()
            .position(|a| a == "ramfb")
            .expect("argv contains the ramfb device");
        assert_eq!(argv[pos - 1], "-device");
    }

    #[test]
    fn argv_without_net_devices_attaches_no_virtio_net() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(
            !argv.iter().any(|a| a.starts_with("virtio-net-device")),
            "a network-free spec must not attach a virtio-net device"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("dgram,id=net")),
            "a network-free spec must not attach a dgram netdev"
        );
    }

    #[test]
    fn argv_without_keyboard_attaches_no_input_device() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));
        assert!(
            !argv.iter().any(|a| a == "virtio-keyboard-device"),
            "an input-free spec must not attach a virtio-keyboard device"
        );
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
    fn argv_attaches_each_net_device_as_virtio_net_mmio() {
        let mut spec = fixture_spec(1);
        spec.net_devices = vec![
            crate::NetDevice {
                qemu_sock: PathBuf::from("/tmp/net0.qemu.sock"),
                peer_sock: PathBuf::from("/tmp/net0.peer.sock"),
                pcap: None,
                mac: None,
            },
            crate::NetDevice {
                qemu_sock: PathBuf::from("/tmp/net1.qemu.sock"),
                peer_sock: PathBuf::from("/tmp/net1.peer.sock"),
                pcap: Some(PathBuf::from("/tmp/cap1.pcap")),
                mac: None,
            },
        ];
        let argv = render(&build_argv(&spec, Path::new("/tmp/k.elf")));

        assert!(argv.iter().any(|a| a
            == "dgram,id=net0,local.type=unix,local.path=/tmp/net0.qemu.sock,\
                remote.type=unix,remote.path=/tmp/net0.peer.sock"));
        assert!(argv.iter().any(|a| a
            == "dgram,id=net1,local.type=unix,local.path=/tmp/net1.qemu.sock,\
                remote.type=unix,remote.path=/tmp/net1.peer.sock"));
        assert!(argv.iter().any(|a| a == "virtio-net-device,netdev=net0"));
        assert!(argv.iter().any(|a| a == "virtio-net-device,netdev=net1"));
        assert!(
            !argv
                .iter()
                .any(|a| a.contains("filter-dump") && a.contains("dump0")),
            "capture-free interface must not attach a filter-dump"
        );
        assert!(argv.iter().any(|a| a.contains("filter-dump")
            && a.contains("netdev=net1")
            && a.contains("/tmp/cap1.pcap")));
    }
}
