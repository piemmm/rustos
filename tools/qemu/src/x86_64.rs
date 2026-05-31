//! x86_64-specific QEMU defaults and argv assembly (Stage 3a (d1)).
//!
//! The generic [`crate::Spec`] is architecture-neutral; everything that
//! is *only* meaningful when targeting `qemu-system-x86_64` lives here:
//!
//! * the default guest RAM size,
//! * the `isa-debug-exit` I/O-port constants,
//! * OVMF / UEFI pflash discovery (`crate::iso::find_ovmf`),
//! * the GRUB BIOS ISO build (`crate::iso::build_grub_iso`),
//! * the exact QEMU argv the runner emits.
//!
//! Splitting this surface out keeps [`crate::Spec`] honest as a
//! per-arch tagged union and lines the codebase up for the Stage 3b/3c/
//! 3d modules (`aarch64.rs`, `riscv64.rs`, `wasm32.rs`) without
//! duplicating any glue (`AGENTS.md` §2.2 / §2.4 — no duplication, no
//! interface creep).
//!
//! # No `unwrap` / `expect` / `panic!`
//!
//! Every fallible call site propagates an `io::Result` per AGENTS.md
//! §2.9. The only `expect`s in this file live inside `#[cfg(test)]`
//! blocks per §2.9's tests carve-out.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::iso::{self, OvmfPaths};
use crate::Spec;

/// Default guest RAM size in mebibytes for an x86_64 QEMU integration
/// test.
///
/// 256 MiB is comfortable headroom for OVMF + GRUB + the test kernel;
/// smaller values trip OVMF's own minimum-RAM checks on some
/// distributions. Callers cannot override this today; if a future test
/// needs more RAM the right move is to add a `with_ram_mib(n)` builder
/// on [`Spec`] rather than smuggling it through `extra_args`.
pub const DEFAULT_RAM_MIB: u32 = 256;

/// I/O port the QEMU `isa-debug-exit` device listens on for x86_64
/// tests.
///
/// Re-exported from [`crate`] for callers that want the value without
/// pulling in the rest of the runner.
pub const ISA_DEBUG_EXIT_IOPORT: u16 = 0xf4;

/// I/O port size the QEMU `isa-debug-exit` device is configured with.
///
/// Re-exported from [`crate`] for callers that want the value without
/// pulling in the rest of the runner.
pub const ISA_DEBUG_EXIT_IOSIZE: u8 = 0x04;

/// Name of the `qemu-system-*` binary for x86_64.
pub const QEMU_BINARY: &str = "qemu-system-x86_64";

/// Build the bootable artifact (a GRUB BIOS ISO containing the
/// multiboot2 kernel) the QEMU invocation needs.
///
/// `staging_dir` and `iso_path` are derived from the kernel's parent
/// directory, mirroring the previous in-tree behaviour exactly so the
/// two existing integration tests (`memory_isolation`,
/// `scheduler_stress_qemu`) see the same on-disk layout under
/// `target/x86_64-unknown-none/debug/`.
///
/// # Errors
///
/// * Propagates every error from [`crate::iso::build_grub_iso`].
pub(crate) fn build_boot_artifact(spec: &Spec) -> io::Result<PathBuf> {
    let kernel_dir = spec
        .kernel
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let stem = spec.kernel.file_stem().unwrap_or_default().to_owned();
    let stem = stem.to_string_lossy();
    let staging = kernel_dir.join(format!("{stem}.grub-staging"));
    let iso_path = kernel_dir.join(format!("{stem}.iso"));
    iso::build_grub_iso(&spec.kernel, &staging, &iso_path)
}

/// Push the x86_64 QEMU argv onto `cmd`.
///
/// Discovers OVMF on the host (see [`crate::iso::find_ovmf`]) and emits
/// the canonical x86_64 invocation: UEFI pflash pair (read-only CODE +
/// writable VARS copy), headless display, serial over stdio,
/// `isa-debug-exit` device on [`ISA_DEBUG_EXIT_IOPORT`], `-m
/// {DEFAULT_RAM_MIB}M`, `-smp {spec.cpus}`, `-no-reboot`, and the boot
/// ISO as a CD-ROM.
///
/// # Errors
///
/// * Propagates every error from [`crate::iso::find_ovmf`] (typically
///   `NotFound` when OVMF is not installed).
pub(crate) fn push_argv(cmd: &mut Command, spec: &Spec, iso: &Path) -> io::Result<()> {
    let ovmf = iso::find_ovmf()?;
    let argv = build_argv(spec, &ovmf, iso);
    for arg in argv {
        cmd.arg(arg);
    }
    Ok(())
}

/// Pure argv builder used by [`push_argv`] and the host unit tests.
///
/// Splitting the pure builder out keeps the argv-assembly contract
/// unit-testable on hosts that do not have OVMF installed (CI runners
/// without the `ovmf` package). The list is intentionally returned as
/// `Vec<OsString>` so callers can inspect it before spawning QEMU.
fn build_argv(spec: &Spec, ovmf: &OvmfPaths, iso: &Path) -> Vec<OsString> {
    // Note: `-nographic` is *not* used because it implicitly attaches the
    // monitor and serial 0 to stdio, which collides with our explicit
    // `-serial stdio`. `-display none` gives the headless behaviour we
    // want without that implicit muxing.
    let mut argv: Vec<OsString> = Vec::with_capacity(20 + spec.extra_args.len());
    argv.push("-no-reboot".into());
    argv.push("-display".into());
    argv.push("none".into());
    argv.push("-serial".into());
    argv.push("stdio".into());
    argv.push("-m".into());
    argv.push(format!("{DEFAULT_RAM_MIB}M").into());
    argv.push("-smp".into());
    argv.push(spec.cpus.to_string().into());
    argv.push("-device".into());
    argv.push(
        format!(
            "isa-debug-exit,iobase=0x{ISA_DEBUG_EXIT_IOPORT:x},\
             iosize=0x{ISA_DEBUG_EXIT_IOSIZE:x}"
        )
        .into(),
    );
    // Attach QEMU's `ramfb` display device when requested. `ramfb` is a
    // firmware-programmed linear framebuffer whose scan-out surface lives
    // in guest RAM; the guest programs its geometry over the `fw_cfg`
    // device the `pc`/`q35` machine already carries (here over the x86
    // IOport DMA interface). This is what the vesa-display vertical
    // drives.
    if spec.display_ramfb {
        argv.push("-device".into());
        argv.push("ramfb".into());
    }
    argv.push("-drive".into());
    argv.push(
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            ovmf.code.display()
        )
        .into(),
    );
    argv.push("-drive".into());
    argv.push(format!("if=pflash,format=raw,file={}", ovmf.vars_copy.display()).into());
    argv.push("-cdrom".into());
    argv.push(iso.into());

    // Confine all PCI BARs to the 32-bit MMIO hole below 4 GiB. The
    // kernel's boot trampoline identity-maps only `0..4 GiB`, and the
    // Stage 4.D `DirectPhysMap` the driver host resolves register
    // windows through covers the same range; a virtio function whose
    // 64-bit BAR firmware placed above 4 GiB would be unreachable.
    //
    // OVMF performs its own PCI enumeration and ignores the host
    // bridge's `pci-hole64-size`, so the knob that matters is OVMF's
    // own `X-PciMmio64Mb` fw_cfg: sizing the 64-bit MMIO window to 0
    // makes OVMF assign every BAR — including 64-bit ones — inside the
    // 32-bit hole. Scope it to specs that actually attach a PCI device
    // so the bring-up-only tests keep the stock firmware configuration.
    if !spec.block_devices.is_empty() || !spec.net_devices.is_empty() {
        argv.push("-fw_cfg".into());
        argv.push("name=opt/ovmf/X-PciMmio64Mb,string=0".into());
    }

    // Attach each backing image as a modern virtio-blk-pci function.
    // `if=none` detaches the drive from any automatic controller so the
    // explicit `-device virtio-blk-pci,drive=blkN` is the only thing that
    // surfaces it to the guest — that is the PCI function the Stage 4.D
    // boot walk discovers and `PciTransport` drives.
    //
    // `disable-legacy=on` forces the function to be a *non-transitional*
    // (modern, virtio-1.0+) device: it reports PCI device id 0x1042
    // (`0x1040 + virtio-blk`) and exposes its registers exclusively
    // through the virtio-1.x PCI capability layout the boot walk decodes
    // (`rustos_kernel::provision_virtio_pci`). Without it QEMU's default
    // `pc`/`q35` machine presents a *transitional* device (id 0x1001) on
    // the legacy PCI bus, which the modern-only walk would not match.
    for (i, dev) in spec.block_devices.iter().enumerate() {
        argv.push("-drive".into());
        let mut drive = OsString::from(format!("if=none,format=raw,id=blk{i},file="));
        drive.push(dev.image.as_os_str());
        argv.push(drive);
        argv.push("-device".into());
        argv.push(format!("virtio-blk-pci,drive=blk{i},disable-legacy=on").into());
    }

    // Attach each network interface as a modern virtio-net-pci function
    // behind a user-mode (SLIRP) backend. `-netdev user` needs no host
    // privileges and presents the fixed `10.0.2.0/24` topology the
    // kernel-side ARP/ICMP test relies on; `disable-legacy=on` pins the
    // function to the modern virtio-1.x PCI layout the Stage 4.D boot
    // walk decodes (device id 0x1041 = `0x1040 + virtio-net`), exactly
    // as for virtio-blk above. An optional `filter-dump` mirrors every
    // frame on the interface to a host pcap so the harness can verify
    // the exchange after the run.
    for (i, dev) in spec.net_devices.iter().enumerate() {
        argv.push("-netdev".into());
        argv.push(format!("user,id=net{i}").into());
        argv.push("-device".into());
        argv.push(format!("virtio-net-pci,netdev=net{i},disable-legacy=on").into());
        if let Some(pcap) = &dev.pcap {
            argv.push("-object".into());
            let mut filter = OsString::from(format!("filter-dump,id=dump{i},netdev=net{i},file="));
            filter.push(pcap.as_os_str());
            argv.push(filter);
        }
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arch;
    use std::time::Duration;

    fn fixture_spec(cpus: u32) -> Spec {
        Spec {
            arch: Arch::X86_64,
            kernel: PathBuf::from("/tmp/k.elf"),
            cpus,
            timeout: Duration::from_secs(60),
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            display_ramfb: false,
            extra_args: Vec::new(),
        }
    }

    fn fixture_ovmf() -> OvmfPaths {
        OvmfPaths {
            code: PathBuf::from("/fake/OVMF_CODE.fd"),
            vars_copy: PathBuf::from("/fake/OVMF_VARS_copy.fd"),
        }
    }

    fn render(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_ram_is_two_hundred_fifty_six_mebibytes() {
        // Pinned at 256 MiB — see the const's docs for why smaller
        // values trip OVMF's minimum-RAM check on some distros.
        assert_eq!(DEFAULT_RAM_MIB, 256);
    }

    #[test]
    fn isa_debug_exit_constants_match_qemu_documentation() {
        // QEMU's `isa-debug-exit` defaults are iobase=0x501,iosize=2;
        // the runner explicitly overrides both. The kernel side
        // (`kernel/arch/x86_64::qemu_exit`) hard-codes the same values
        // — a mismatch here is a silent test-protocol break.
        assert_eq!(ISA_DEBUG_EXIT_IOPORT, 0xf4);
        assert_eq!(ISA_DEBUG_EXIT_IOSIZE, 0x04);
    }

    #[test]
    fn qemu_binary_name_is_arch_specific() {
        assert_eq!(QEMU_BINARY, "qemu-system-x86_64");
    }

    #[test]
    fn argv_contains_documented_invariant_flags() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        // Headless boot — see the comment on `build_argv` for why
        // `-nographic` is forbidden.
        assert!(argv.iter().any(|a| a == "-no-reboot"));
        assert!(argv.iter().any(|a| a == "-display"));
        assert!(argv.iter().any(|a| a == "none"));
        assert!(argv.iter().any(|a| a == "-serial"));
        assert!(argv.iter().any(|a| a == "stdio"));
    }

    #[test]
    fn argv_encodes_ram_size_in_mebibytes() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
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
            let argv = render(&build_argv(
                &spec,
                &fixture_ovmf(),
                Path::new("/tmp/out.iso"),
            ));
            let pos = argv
                .iter()
                .position(|a| a == "-smp")
                .expect("argv contains -smp");
            assert_eq!(argv[pos + 1], n.to_string());
        }
    }

    #[test]
    fn argv_programs_isa_debug_exit_with_runner_constants() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        let pos = argv
            .iter()
            .position(|a| a == "-device")
            .expect("argv contains -device");
        let device = &argv[pos + 1];
        assert!(
            device.starts_with("isa-debug-exit,"),
            "expected isa-debug-exit device, got {device}"
        );
        assert!(
            device.contains(&format!("iobase=0x{ISA_DEBUG_EXIT_IOPORT:x}")),
            "device string missing runner ioport: {device}"
        );
        assert!(
            device.contains(&format!("iosize=0x{ISA_DEBUG_EXIT_IOSIZE:x}")),
            "device string missing runner iosize: {device}"
        );
    }

    #[test]
    fn argv_attaches_ovmf_pflash_pair_in_documented_order() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));

        let mut drives: Vec<&String> = Vec::new();
        for (i, a) in argv.iter().enumerate() {
            if a == "-drive" {
                drives.push(&argv[i + 1]);
            }
        }
        assert_eq!(drives.len(), 2, "expected exactly two -drive entries");

        // First pflash slot is the read-only CODE image; QEMU pairs the
        // writable VARS image with the second pflash slot. Swapping the
        // two boots an OVMF instance that immediately faults — encode
        // the ordering in a test rather than as a comment.
        assert!(
            drives[0].contains("readonly=on") && drives[0].contains("OVMF_CODE.fd"),
            "first -drive must be the read-only CODE image, got {}",
            drives[0]
        );
        assert!(
            !drives[1].contains("readonly=on") && drives[1].contains("OVMF_VARS_copy.fd"),
            "second -drive must be the writable VARS copy, got {}",
            drives[1]
        );
    }

    #[test]
    fn argv_without_block_devices_attaches_no_virtio_blk() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        assert!(
            !argv.iter().any(|a| a.starts_with("virtio-blk-pci")),
            "a storage-free spec must not attach a virtio-blk device"
        );
    }

    #[test]
    fn argv_attaches_each_block_device_as_virtio_blk_pci() {
        let mut spec = fixture_spec(1);
        spec.block_devices = vec![
            crate::BlockDevice {
                image: PathBuf::from("/tmp/disk0.img"),
            },
            crate::BlockDevice {
                image: PathBuf::from("/tmp/disk1.img"),
            },
        ];
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));

        // Each device is a detached drive (`if=none`) bound to its own
        // virtio-blk-pci function by a matching id.
        assert!(argv.iter().any(|a| a.contains("if=none")
            && a.contains("id=blk0")
            && a.contains("/tmp/disk0.img")));
        assert!(argv.iter().any(|a| a.contains("if=none")
            && a.contains("id=blk1")
            && a.contains("/tmp/disk1.img")));
        // `disable-legacy=on` pins the function to the modern
        // (non-transitional) virtio-1.x layout the boot walk decodes.
        assert!(argv
            .iter()
            .any(|a| a == "virtio-blk-pci,drive=blk0,disable-legacy=on"));
        assert!(argv
            .iter()
            .any(|a| a == "virtio-blk-pci,drive=blk1,disable-legacy=on"));
    }

    #[test]
    fn argv_without_ramfb_attaches_no_display_device() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        assert!(
            !argv.iter().any(|a| a == "ramfb"),
            "a display-free spec must not attach a ramfb device"
        );
    }

    #[test]
    fn argv_attaches_ramfb_when_requested() {
        let mut spec = fixture_spec(1);
        spec.display_ramfb = true;
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        let pos = argv
            .iter()
            .position(|a| a == "ramfb")
            .expect("argv contains the ramfb device");
        assert_eq!(argv[pos - 1], "-device");
    }

    #[test]
    fn argv_without_net_devices_attaches_no_virtio_net() {
        let spec = fixture_spec(1);
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        assert!(
            !argv.iter().any(|a| a.starts_with("virtio-net-pci")),
            "a network-free spec must not attach a virtio-net device"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("user,id=net")),
            "a network-free spec must not attach a user netdev"
        );
    }

    #[test]
    fn argv_attaches_each_net_device_as_virtio_net_pci() {
        let mut spec = fixture_spec(1);
        spec.net_devices = vec![
            crate::NetDevice::default(),
            crate::NetDevice {
                pcap: Some(PathBuf::from("/tmp/cap1.pcap")),
            },
        ];
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));

        // Each interface is a user-mode netdev bound to its own modern
        // virtio-net-pci function by a matching id.
        assert!(argv.iter().any(|a| a == "user,id=net0"));
        assert!(argv.iter().any(|a| a == "user,id=net1"));
        assert!(argv
            .iter()
            .any(|a| a == "virtio-net-pci,netdev=net0,disable-legacy=on"));
        assert!(argv
            .iter()
            .any(|a| a == "virtio-net-pci,netdev=net1,disable-legacy=on"));
        // Only the interface with a capture path gets a filter-dump.
        assert!(
            !argv
                .iter()
                .any(|a| a.contains("filter-dump") && a.contains("net0")),
            "capture-free interface must not attach a filter-dump"
        );
        assert!(argv.iter().any(|a| a.contains("filter-dump")
            && a.contains("netdev=net1")
            && a.contains("/tmp/cap1.pcap")));
    }

    #[test]
    fn argv_confines_pci_bars_below_4gib_for_net_only_specs() {
        // A net-only spec still attaches a PCI function, so the
        // X-PciMmio64Mb=0 fw_cfg that keeps BARs reachable from the boot
        // identity map must fire even with no block devices.
        let mut spec = fixture_spec(1);
        spec.net_devices = vec![crate::NetDevice::default()];
        let argv = render(&build_argv(
            &spec,
            &fixture_ovmf(),
            Path::new("/tmp/out.iso"),
        ));
        assert!(argv
            .iter()
            .any(|a| a == "name=opt/ovmf/X-PciMmio64Mb,string=0"));
    }

    #[test]
    fn argv_passes_cdrom_iso_last_among_boot_flags() {
        let spec = fixture_spec(1);
        let iso = Path::new("/tmp/out.iso");
        let argv = render(&build_argv(&spec, &fixture_ovmf(), iso));
        let pos = argv
            .iter()
            .position(|a| a == "-cdrom")
            .expect("argv contains -cdrom");
        assert_eq!(argv[pos + 1], iso.to_string_lossy().into_owned());
    }
}
