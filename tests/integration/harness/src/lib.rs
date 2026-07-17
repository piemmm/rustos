//! Build-time target classification shared by the freestanding QEMU
//! integration binaries.
//!
//! The integration binaries under `tests/integration/` compile two ways:
//! as freestanding `no_std`/`no_main` kernels for a bare-metal QEMU
//! target, and as inert host stubs for `cargo build --workspace`. They
//! must choose between those forms without naming the target instruction
//! set in their own source, which confines to the architecture
//! ports and the build glue.
//!
//! This crate is that build glue. Each binary's build script calls
//! [`emit_target_cfg`], which inspects the cargo-provided target
//! description and enables the matching conditional-compilation names:
//!
//! * `freestanding` — the crate is being built for a bare-metal
//!   (`os = "none"`) target and should compile its kernel body.
//! * `itest_x86_64` — freestanding on the 64-bit x86 port.
//! * `itest_riscv64` — freestanding on the 64-bit RISC-V port.
//! * `itest_aarch64` — freestanding on the 64-bit Arm port.
//! * `itest_wasm32` — the browser-sandbox wasm32 port
//!   (`wasm32-unknown-unknown`, `os = "unknown"`). Unlike the bare-metal
//!   ports this is a `cdylib`, not a `no_main` kernel, so it gets its
//!   own cfg *without* `freestanding`.
//!
//! Binaries gate on those names instead of a raw target predicate, so the
//! instruction-set choice lives in this one audited place.
//!
//! It also hosts the build-time [`elf2rxe`] converter, which turns a linked
//! PIE program ELF into the `rxe` load image the kernel spawn path consumes
//! (used by the CCOMPAT CC3 spawn round-trips).

pub mod elf2rxe;

/// The freestanding cross-compile target vocabulary (`PieArch`): the one
/// definition of each Tier-1 target's Rust triple and its
/// `CARGO_TARGET_<triple>_RUSTFLAGS` variable, shared by the `tools/xtask`
/// image pipeline and the autoload-root fixture's build script so the arch
/// selection cannot drift between them.
pub mod pie;

/// Dep-info-driven `cargo:rerun-if-changed` emission for build scripts that
/// run an inner `cargo build` and embed its output: freshness is derived
/// from the compiler's own dep-info record, never a hand-kept source list
/// that rots and ships a stale embedded binary.
pub mod dep_info;

/// The M1 demand-paged file-mapping fixture: the single definition of the
/// fixture file the `file_map_qemu_*` verticals serve kernel-side and probe
/// from EL0 (geometry constants, content generator, `TAIRIX_FM_*` env
/// pinning, and the kernel-side constants emitter).
pub mod filemap_fixture;

/// The signed `.rxe` driver-bundle composer, shared by the build scripts
/// that lay a kernel-trusted driver into the system.
/// Enabled by the `driver-image` feature so the Ed25519 dependency is
/// pulled in only where a bundle is actually signed.
#[cfg(feature = "driver-image")]
pub mod driver_image;

/// The signed application-bundle composer and `AppInfo.toml` manifest
/// discovery, shared by every build that plants a program bundle onto the
/// read-only `/System` store (`plans/APPS.md` deliverable 8).
/// Enabled by the `app-image` feature so the signing/hashing dependencies
/// are pulled in only where a bundle is actually composed.
#[cfg(feature = "app-image")]
pub mod app_image;

/// Virtual base the production aarch64 spawn producer maps every spawned
/// user image at — the `SHELL_USER_BIAS` (64 GiB) the kernel's `build.rs`
/// bakes into `spawn_layout`.
///
/// A `.rxe` the kernel spawn path will load must have its `R_*_RELATIVE`
/// relocations baked for this exact bias ([`elf2rxe::elf_to_rxe`]'s
/// `load_bias`), so the converted image runs correctly once mapped at
/// `vaddr + USER_IMAGE_BIAS`. It is the one definition every build script
/// that bakes a spawnable `rxe` shares; the kernel spawn
/// path asserts the baked bias matches `SHELL_USER_BIAS` and fails closed on
/// a mismatch, so a drift between this constant and the
/// kernel is caught rather than miscompiled.
pub const USER_IMAGE_BIAS: u64 = 0x10_0000_0000;

/// Cargo environment key naming the target operating system.
const TARGET_OS_KEY: &str = "CARGO_CFG_TARGET_OS";
/// Cargo environment key naming the target instruction set.
const TARGET_ARCH_KEY: &str = "CARGO_CFG_TARGET_ARCH";

/// Every conditional-compilation name this crate may enable. Declared to
/// the compiler unconditionally so `--cfg`-aware lints accept the gates
/// even on host builds where none of them are active.
pub const KNOWN_CFGS: &[&str] = &[
    "freestanding",
    "itest_x86_64",
    "itest_riscv64",
    "itest_aarch64",
    "itest_wasm32",
];

/// Classify a target into the conditional-compilation names its
/// freestanding integration binary should enable.
///
/// Bare-metal targets (`os == "none"`) are freestanding; the matching
/// per-port name is added when the instruction set is one the QEMU
/// verticals cover. Hosted targets enable nothing, leaving the binary as
/// an inert stub.
#[must_use]
pub fn active_cfgs(os: &str, arch: &str) -> Vec<&'static str> {
    // The wasm32 browser target (`wasm32-unknown-unknown`, `os =
    // "unknown"`) is a `cdylib` the host loads, not a bare-metal
    // `no_main` kernel, so it enables its own cfg without
    // `freestanding`.
    if os == "unknown" && arch == "wasm32" {
        return vec!["itest_wasm32"];
    }
    if os != "none" {
        return Vec::new();
    }
    let mut cfgs = vec!["freestanding"];
    match arch {
        "x86_64" => cfgs.push("itest_x86_64"),
        "riscv64" => cfgs.push("itest_riscv64"),
        "aarch64" => cfgs.push("itest_aarch64"),
        _ => {}
    }
    cfgs
}

/// Emit the conditional-compilation flags for the current build.
///
/// Call this from a binary's build script. It declares every
/// [`KNOWN_CFGS`] name to the compiler and enables those returned by
/// [`active_cfgs`] for the target cargo is building.
pub fn emit_target_cfg() {
    for name in KNOWN_CFGS {
        println!("cargo:rustc-check-cfg=cfg({name})");
    }
    let os = std::env::var(TARGET_OS_KEY).unwrap_or_default();
    let arch = std::env::var(TARGET_ARCH_KEY).unwrap_or_default();
    for name in active_cfgs(&os, &arch) {
        println!("cargo:rustc-cfg={name}");
    }
}

/// Build the `qemu-system-aarch64` argument vector that dumps the
/// canonical `virt`-board flattened device tree to `dtb_path` for `cpus`
/// CPUs.
///
/// Split out from [`dump_aarch64_virt_dtb`] so the argument shape is
/// unit-testable without invoking QEMU. The machine matches `tools/qemu`'s
/// aarch64 `virt` definition (`cortex-a72`, 256 MiB); the DTB layout the
/// verticals read from the blob (virtio-MMIO transport bases, GICv2 SPIs,
/// the `/psci` conduit) is the stable `virt`-board layout, independent of
/// the CPU count.
#[must_use]
pub fn dump_virt_dtb_args(dtb_path: &str, cpus: u32) -> Vec<String> {
    vec![
        "-M".to_string(),
        format!("virt,dumpdtb={dtb_path}"),
        "-cpu".to_string(),
        "cortex-a72".to_string(),
        "-m".to_string(),
        "256M".to_string(),
        "-smp".to_string(),
        cpus.to_string(),
        "-display".to_string(),
        "none".to_string(),
        "-no-reboot".to_string(),
    ]
}

/// Dump the canonical QEMU `virt`-board flattened device tree for `cpus`
/// CPUs into `OUT_DIR` and return its (trimmed) bytes, so a freestanding
/// QEMU vertical can embed it.
///
/// QEMU's `-kernel <ELF>` aarch64 path treats the image as bare firmware
/// and passes no DTB pointer to the kernel (`x0 = 0`, unlike the Linux
/// Image protocol), so a vertical that needs the board's device tree at
/// runtime embeds this blob instead of reading a live pointer. This lives
/// here **once** rather than copied into every aarch64 build script
/// (no duplication).
///
/// QEMU's `dumpdtb` pads the blob to the machine's 1 MiB device-tree
/// region; the bytes are passed through [`trim_fdt_to_extent`] so the
/// embedded copy is only the meaningful FDT and a vertical that links it
/// is not bloated by ~1 MiB of zero padding.
///
/// # Panics
///
/// Panics if `qemu-system-aarch64` cannot be spawned, exits non-zero, or
/// the dumped file cannot be read. A build script cannot proceed without
/// the blob, so failing loudly is correct.
#[must_use]
pub fn dump_aarch64_virt_dtb(out_dir: &std::ffi::OsStr, cpus: u32) -> Vec<u8> {
    let dtb_path = std::path::PathBuf::from(out_dir).join("virt.dtb");
    let dtb_str = dtb_path.display().to_string();
    let status = std::process::Command::new("qemu-system-aarch64")
        .args(dump_virt_dtb_args(&dtb_str, cpus))
        .status()
        .expect("run qemu-system-aarch64 to dump the virt DTB");
    assert!(status.success(), "qemu dumpdtb failed: {status}");
    trim_fdt_to_extent(&std::fs::read(&dtb_path).expect("read dumped virt.dtb"))
}

/// Trim a flattened device tree to the extent its header describes,
/// dropping any trailing padding, and rewrite the `totalsize` field to
/// match.
///
/// QEMU's `dumpdtb` emits the blob padded out to the machine's 1 MiB
/// device-tree region. A reader only needs the memory-reservation,
/// structure, and strings blocks, so this returns the prefix up to the
/// furthest block end (`off_dt_struct + size_dt_struct` /
/// `off_dt_strings + size_dt_strings`) and patches `totalsize` so the
/// trimmed copy stays self-consistent for a `totalsize`-driven reader.
///
/// A blob too short for the 40-byte header, with the wrong magic, or
/// whose header offsets escape the buffer is returned unchanged — trimming
/// is an optimisation, never a parser (: callers still
/// validate the result through `tairix_fdt::Fdt::new`).
#[must_use]
pub fn trim_fdt_to_extent(bytes: &[u8]) -> Vec<u8> {
    const FDT_MAGIC: u32 = 0xd00d_feed;
    let be_u32 = |off: usize| -> Option<u32> {
        let s = bytes.get(off..off + 4)?;
        Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let header_ok = bytes.len() >= 40 && be_u32(0) == Some(FDT_MAGIC);
    let extent = header_ok.then(|| {
        let struct_off = be_u32(8)? as usize;
        let strings_off = be_u32(12)? as usize;
        let strings_size = be_u32(32)? as usize;
        let struct_size = be_u32(36)? as usize;
        let struct_end = struct_off.checked_add(struct_size)?;
        let strings_end = strings_off.checked_add(strings_size)?;
        let end = struct_end.max(strings_end);
        (end <= bytes.len()).then_some(end)
    });
    match extent.flatten() {
        Some(end) if end < bytes.len() => {
            let mut trimmed = bytes[..end].to_vec();
            let total = u32::try_from(end).unwrap_or(u32::MAX).to_be_bytes();
            trimmed[4..8].copy_from_slice(&total);
            trimmed
        }
        _ => bytes.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_targets_are_inert() {
        assert!(active_cfgs("linux", "x86_64").is_empty());
        assert!(active_cfgs("macos", "aarch64").is_empty());
    }

    #[test]
    fn bare_metal_x86_64_is_freestanding() {
        assert_eq!(
            active_cfgs("none", "x86_64"),
            ["freestanding", "itest_x86_64"]
        );
    }

    #[test]
    fn bare_metal_riscv64_is_freestanding() {
        assert_eq!(
            active_cfgs("none", "riscv64"),
            ["freestanding", "itest_riscv64"]
        );
    }

    #[test]
    fn bare_metal_aarch64_is_freestanding() {
        assert_eq!(
            active_cfgs("none", "aarch64"),
            ["freestanding", "itest_aarch64"]
        );
    }

    #[test]
    fn unknown_bare_metal_arch_is_freestanding_only() {
        assert_eq!(active_cfgs("none", "wasm32"), ["freestanding"]);
    }

    #[test]
    fn wasm32_browser_target_is_a_cdylib_not_freestanding() {
        assert_eq!(active_cfgs("unknown", "wasm32"), ["itest_wasm32"]);
    }

    #[test]
    fn every_active_cfg_is_declared() {
        for (os, arch) in [("none", "x86_64"), ("none", "riscv64"), ("none", "wasm32")] {
            for name in active_cfgs(os, arch) {
                assert!(KNOWN_CFGS.contains(&name), "{name} not declared");
            }
        }
    }

    #[test]
    fn dump_virt_dtb_args_match_the_runner_machine() {
        let args = dump_virt_dtb_args("/tmp/out/virt.dtb", 2);
        assert_eq!(
            args,
            [
                "-M",
                "virt,dumpdtb=/tmp/out/virt.dtb",
                "-cpu",
                "cortex-a72",
                "-m",
                "256M",
                "-smp",
                "2",
                "-display",
                "none",
                "-no-reboot",
            ]
        );
    }

    #[test]
    fn dump_virt_dtb_args_render_the_cpu_count() {
        let one = dump_virt_dtb_args("d", 1);
        let four = dump_virt_dtb_args("d", 4);
        let smp = |a: &[String]| a[a.iter().position(|s| s == "-smp").unwrap() + 1].clone();
        assert_eq!(smp(&one), "1");
        assert_eq!(smp(&four), "4");
    }

    #[test]
    fn trimming_drops_padding_and_keeps_the_tree_parseable() {
        let blob = tairix_fdt::fixture::virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        // Simulate QEMU `dumpdtb` padding the blob out to its 1 MiB region.
        let mut padded = blob.clone();
        padded.resize(blob.len() + 4096, 0);

        let trimmed = trim_fdt_to_extent(&padded);
        assert!(trimmed.len() < padded.len(), "padding was not dropped");
        assert!(trimmed.len() <= blob.len());

        let fdt = tairix_fdt::Fdt::new(&trimmed).expect("trimmed fdt parses");
        assert_eq!(fdt.first_memory_region(), Some((0x4000_0000, 0x2000_0000)));
        let method = fdt
            .property(&[b"psci"], b"method")
            .expect("psci method present after trim");
        assert!(method.starts_with(b"hvc"), "psci method survived trim");
    }

    #[test]
    fn trimming_rewrites_totalsize_to_the_trimmed_length() {
        let blob = tairix_fdt::fixture::virt_like_arm(0x4000_0000, 0x2000_0000, "smc", 30);
        let mut padded = blob.clone();
        padded.resize(blob.len() + 8192, 0);
        let trimmed = trim_fdt_to_extent(&padded);
        let total = u32::from_be_bytes([trimmed[4], trimmed[5], trimmed[6], trimmed[7]]) as usize;
        assert_eq!(total, trimmed.len(), "totalsize must match trimmed length");
    }

    #[test]
    fn trimming_leaves_a_short_or_non_fdt_blob_unchanged() {
        assert_eq!(trim_fdt_to_extent(&[1, 2, 3]), vec![1, 2, 3]);
        let mut not_fdt = vec![0u8; 64];
        not_fdt[0] = 0xab;
        assert_eq!(trim_fdt_to_extent(&not_fdt), not_fdt);
    }
}
