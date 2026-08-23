//! `cargo xtask fuzz` — drive the in-tree fuzz harnesses.
//!
//! TAIRiX does not pull in an external fuzz runner: the
//! per-crate harnesses are seeded, allocation-free Rust tests that
//! explicitly sanctions as the "equivalent in-tree harness". This
//! orchestrator is the single place that runs every such harness for a
//! wall-clock budget, so a PR and a nightly soak share one definition of the
//! target set.
//!
//! Each [`Target`] names an existing `cargo test` integration harness. The
//! orchestrator exports `TAIRIX_FUZZ_BUDGET_SECS`, which the harness reads to
//! keep drawing fresh inputs from its continuing PRNG stream until the budget
//! elapses (a plain `cargo test` leaves the variable unset and runs the fast,
//! fixed-iteration smoke sweep instead). It also exports a per-harness PRNG
//! seed (`commands::seed`): by default a *fresh* seed each run, so two soaks
//! never replay the same input stream; with `--seed N`, a deterministic
//! seed that reproduces a logged crash. The chosen seed is logged with each
//! job. A harness that crashes, hangs, or fails its invariant fails the
//! command fails closed.
//!
//! Adding a harness means adding a [`Target`] here, never teaching `ci`
//! about it directly. The burn-down now covers the wire decoders,
//! the syscall dispatcher, the `lib/net` protocol parsers, and the
//! capability-checked IPC port endpoint.

use std::ffi::OsString;
use std::time::Duration;

use crate::commands::parallel::{self, Job};
use crate::commands::seed;
use crate::Context;

/// One in-tree fuzz harness the orchestrator knows how to run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Workspace package that owns the harness (`cargo test -p`).
    pub package: &'static str,
    /// Integration-test binary name (the `tests/<name>.rs` file stem).
    pub test: &'static str,
    /// One-line description shown by `cargo xtask fuzz --list`.
    pub description: &'static str,
}

/// The closed set of fuzz harnesses, in run order.
///
/// Every entry is a deterministic `cargo test` harness; this registry is
/// what wires them into the budgeted runs. The set covers the wire
/// decoders (`lib/abi`), the syscall dispatcher, the `lib/net`
/// protocol parsers, and the capability-checked IPC port endpoint.
pub const TARGETS: &[Target] = &[
    Target {
        package: "tairix-arch-api",
        test: "fuzz_backtrace",
        description: "Arch HAL frame-pointer stack unwinder (panic backtrace)",
    },
    Target {
        package: "tairix-abi",
        test: "fuzz_decode",
        description: "lib/abi wire decoders (IPC + manifest headers)",
    },
    Target {
        package: "tairix-fontface",
        test: "fuzz_face",
        description: "lib/fontface TrueType parser + outline rasteriser",
    },
    Target {
        package: "tairix-kernel-syscall",
        test: "fuzz_args",
        description: "syscall dispatcher argument validation",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_eth",
        description: "lib/net Ethernet/ARP/IPv4/ICMP wire codecs",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_addr",
        description: "lib/net address scope/zone rules + Internet-checksum properties",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_ipv4",
        description: "lib/net IPv4 codec, emit fragmentation, fragment reassembly budgets",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_ipv6",
        description: "lib/net IPv6 codec + extension-header chain walk",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_icmp",
        description: "lib/net ICMP/ICMPv6 echo + error codecs, error rate limiter",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_nd",
        description: "lib/net Neighbour Discovery codecs + neighbour-table glue",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_stack",
        description: "lib/net dual-stack host engine frame entry point",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_udp",
        description: "lib/net dual-stack UDP codec (pseudo-header checksum, length bounds)",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_tcp",
        description: "lib/net TCP segment codec (header, options, pseudo-header checksum)",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_igmp",
        description: "lib/net IGMPv2 multicast-membership codec (checksum, type, length bounds)",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_mld",
        description: "lib/net MLDv2 query decode + report encode (floating max-response, bounds)",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_dhcp",
        description: "lib/net DHCPv4 client codec + state machine (reply decode, transitions)",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_dhcpv6",
        description: "lib/net DHCPv6 client codec + state machine (RFC 8415 reply decode, transitions)",
    },
    Target {
        package: "tairix-net",
        test: "fuzz_net_dns",
        description: "lib/net DNS stub resolver (RFC 1035 name/message decode, resolver state machine)",
    },
    Target {
        package: "tairix-netstack",
        test: "fuzz_net_sockabi",
        description: "netstack socket serve path (decode + CAP_NET gate + dispatch, accounting)",
    },
    Target {
        package: "tairix-confd",
        test: "fuzz_vault",
        description: "app-data sealed-scope records (master secret + sealed document)",
    },
    Target {
        package: "tairix-appconf",
        test: "fuzz_appconf",
        description: "per-app configuration engine (grammar, parse/render fixed point, bounds)",
    },
    Target {
        package: "tairix-netconfig",
        test: "fuzz_netconfig",
        description: "network.conf store engine (per-interface parse, bounds, render round-trip)",
    },
    Target {
        package: "tairix-proglib",
        test: "fuzz_proglib",
        description:
            "program-library catalog store (entry grammar, patch merge, render round-trip, bounds)",
    },
    Target {
        package: "tairix-wallpaper",
        test: "fuzz_wallpaper_settings",
        description:
            "pinboard settings store (closed key registry grammar, render round-trip, bounds)",
    },
    Target {
        package: "tairix-kernel-ipc",
        test: "fuzz_port",
        description: "IPC port send dispatch (capability + size + capacity)",
    },
    Target {
        package: "tairix-kernel-mem",
        test: "fuzz_swap",
        description: "encrypted-swap restore path (untrusted swap-device bytes)",
    },
    Target {
        package: "tairix-kernel-mem",
        test: "fuzz_ramzip",
        description: "ramzip compressed-entry restore (tampered/truncated sealed entries)",
    },
    Target {
        package: "tairix-drv-fs-arxfs",
        test: "fuzz_mount",
        description:
            "arxfs mount / metadata + directory decode (superblock ring, root, trees, dirents)",
    },
    Target {
        package: "tairix-drv-fs-adfs",
        test: "fuzz_adfs_mount",
        description:
            "ADFS mount / decode (maps, disc records, boot block, fixed + big directories)",
    },
    Target {
        package: "tairix-raidmeta",
        test: "fuzz_superblock",
        description:
            "RAID array-superblock decode (untrusted on-disk member metadata: magic, bounds, CRC)",
    },
    Target {
        package: "tairix-raidmeta",
        test: "fuzz_maintenance",
        description:
            "RAID maintenance-record decode (untrusted on-disk scrub/rebuild cursors: flags, canonical fields, CRC)",
    },
    Target {
        package: "tairix-compress",
        test: "fuzz_compress",
        description: "first-party LZ decode (untrusted compressed-record bytes)",
    },
    Target {
        package: "tairix-crc32c",
        test: "fuzz_crc32c",
        description: "CRC-32C accelerated candidates vs the portable reference (bit-identity)",
    },
    Target {
        package: "tairix-pagezero",
        test: "fuzz_pagezero",
        description:
            "page-zero accelerated candidates vs the portable reference (zeroes exactly the region, touches nothing past it)",
    },
    Target {
        package: "tairix-svg",
        test: "fuzz_svg",
        description: "SVG asset decode (untrusted /System/Graphics image bytes)",
    },
    Target {
        package: "tairix-image",
        test: "fuzz_image",
        description:
            "raster-image decode (untrusted PNG + JPEG bytes: chunk framing, filters, interlace, marker segments, Huffman scans)",
    },
    Target {
        package: "tairix-virtio",
        test: "fuzz_virtqueue",
        description:
            "split-virtqueue completion path (hostile device-written used ring / descriptors)",
    },
    Target {
        package: "tairix-vt",
        test: "fuzz_vt",
        description: "lib/vt escape-sequence parser (untrusted terminal / remote-host bytes)",
    },
    Target {
        package: "tairix-fdt",
        test: "fuzz_fdt",
        description: "lib/fdt device-tree reader (untrusted firmware/bootloader DTB bytes)",
    },
    Target {
        package: "tairix-partition",
        test: "fuzz_partition",
        description: "lib/partition MBR + GPT table parsers (untrusted on-disk partition bytes)",
    },
    Target {
        package: "tairix-fsprobe",
        test: "fuzz_fsprobe",
        description: "lib/fsprobe filesystem-signature probe (untrusted removable-media bytes)",
    },
    Target {
        package: "tairix-fsmeta",
        test: "fuzz_fsmeta",
        description:
            "lib/fsmeta key-grammar parser + attribute-set decoder (untrusted stored/foreign bytes)",
    },
    Target {
        package: "tairix-curses",
        test: "fuzz_curses_input",
        description: "lib/curses input decoder (untrusted key/mouse/paste bytes)",
    },
    Target {
        package: "tairix-users",
        test: "fuzz_users",
        description: "lib/users database parser (untrusted /System/Security/Users bytes)",
    },
    Target {
        package: "tairix-users",
        test: "fuzz_groups",
        description: "lib/users group-database parser (untrusted /System/Security/Groups bytes)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_field",
        description: "lib/log typed-field value decoder (untrusted log-record field bytes)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_segment",
        description: "lib/log segment decoder (untrusted /System/Logs segment bytes)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_record",
        description: "lib/log logical-record decoder (untrusted /System/Logs record bytes)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_dict",
        description:
            "lib/log segment string-dictionary codec (untrusted /System/Logs dictionary bytes)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_bootring",
        description:
            "lib/log early-boot ring FIFO (push/pop/evict ordering + loss-range accounting)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_ingress",
        description:
            "lib/log record-ingress admission (attested-origin vs caller stream/source spoof + append sequencing)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_journal",
        description:
            "lib/log journal engine (admit/commit/import_boot: rotation, chaining, sealing; every persisted segment must verify)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_ratelimit",
        description:
            "lib/log ingress rate limiter (token-bucket admit/drop + coalesced loss reporting: every drop is accounted, never panics)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_render",
        description:
            "lib/log boot-console renderer (hostile caller text/fields: rendered line is always control-byte-free, never panics)",
    },
    Target {
        package: "tairix-log",
        test: "fuzz_report",
        description:
            "lib/log rich renderers (JSON/Markdown/table over hostile records: output is always control-byte-free, JSON is valid, never panics)",
    },
    Target {
        package: "tairix-glob",
        test: "fuzz_glob",
        description: "lib/glob pattern compiler + match loop (untrusted glob-pattern bytes)",
    },
    Target {
        package: "tairix-help",
        test: "fuzz_help",
        description:
            "lib/help document parser + short/full renderers (untrusted help-document bytes: never panics, fails closed, printed output is control-free)",
    },
    Target {
        package: "tairix-path",
        test: "fuzz_path",
        description: "lib/path path-string parser + canonical-spelling round-trip (untrusted path bytes)",
    },
    Target {
        package: "tairix-resref",
        test: "fuzz_resref",
        description:
            "lib/resref resource-reference parser + canonical-spelling round-trip (untrusted reference bytes)",
    },
    Target {
        package: "tairix-procinfo",
        test: "fuzz_resinfo",
        description:
            "lib/procinfo info:/stats: resolver (untrusted reference bytes + hostile sysinfod replies: never panics, fails closed, envelope invariants hold)",
    },
    Target {
        package: "tairix-devids",
        test: "fuzz_devids",
        description:
            "lib/devids pci.ids/usb.ids vetting parser + compact-table decoder (untrusted upstream-download and table bytes)",
    },
    Target {
        package: "tairix-binfmt",
        test: "fuzz_rxe",
        description:
            "lib/binfmt rxe inspection view + manifest summary (untrusted executable-file bytes)",
    },
    Target {
        package: "tairix-binfmt",
        test: "fuzz_elf",
        description: "lib/binfmt ELF64 view (untrusted executable-file bytes)",
    },
    Target {
        package: "tairix-binfmt",
        test: "fuzz_wasm",
        description: "lib/binfmt wasm module-structure view (untrusted executable-file bytes)",
    },
    Target {
        package: "tairix-bootload",
        test: "fuzz_plan",
        description: "boot-chain loader core (untrusted kernel-image ELF -> LoadPlan validation)",
    },
    Target {
        package: "tairix-multiboot2",
        test: "fuzz_info",
        description: "Multiboot2 information-structure parse + builder round-trip (untrusted boot-info bytes)",
    },
    Target {
        package: "tairix-disasm",
        test: "fuzz_riscv64",
        description: "lib/disasm RV64GC decoder (untrusted executable-file bytes: never panics, always makes forward progress)",
    },
    Target {
        package: "tairix-disasm",
        test: "fuzz_aarch64",
        description: "lib/disasm A64 decoder (untrusted executable-file bytes: never panics, always makes forward progress)",
    },
    Target {
        package: "tairix-disasm",
        test: "fuzz_wasm_isa",
        description: "lib/disasm wasm code-body decoder (untrusted executable-file bytes: never panics, always makes forward progress)",
    },
    Target {
        package: "tairix-disasm",
        test: "fuzz_x86_64",
        description: "lib/disasm x86_64 decoder (untrusted executable-file bytes: never panics, always makes forward progress, 15-byte cap)",
    },
    Target {
        package: "tairix-sandbox",
        test: "fuzz_sandbox",
        description: "lib/sandbox decode seam (hostile input files through the sandboxed decode service, and hostile worker replies into the fail-closed client decoders)",
    },
    Target {
        package: "tairix-supervisor",
        test: "fuzz_repl",
        description:
            "pre-boot Supervisor REPL line/command parser (untrusted physical-console bytes: never panics, always terminates)",
    },
];

/// How long to run each harness.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// `--once`: a single smoke iteration per harness, with a fresh, logged
    /// seed. This is the per-PR `ci` gate: `ci` runs each test once (on a
    /// developer machine and a CI runner alike); the wall-clock soak coverage
    /// is the time-limited GitHub soak, not `ci`.
    Once,
    /// `--quick`: a short wall-clock budget (≥ 5 s per harness), used by the
    /// time-limited GitHub `ci` soak step (`tools/ci/soak.sh`).
    Quick,
    /// `--soak`: the nightly budget (≥ 24 h per harness).
    Soak,
}

impl Mode {
    /// Per-harness wall-clock budget, or `None` for [`Mode::Once`] (a single
    /// smoke iteration with no budget — the harness draws one input).
    #[must_use]
    pub fn budget(self) -> Option<Duration> {
        match self {
            // A single iteration: no wall-clock budget is exported, so the
            // harness runs its single smoke iteration.
            Mode::Once => None,
            // "runs each harness for ≥ 5 s" (the GitHub soak step).
            Mode::Quick => Some(Duration::from_secs(5)),
            // "runs each harness for ≥ 24 h".
            Mode::Soak => Some(Duration::from_hours(24)),
        }
    }
}

/// Parsed `fuzz` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// Selected budget.
    pub mode: Mode,
    /// Optional harness filter (`--target <name>`); runs all when `None`.
    pub only: Option<String>,
    /// `--list`: print the registry and exit without running anything.
    pub list: bool,
    /// Override the per-harness budget in seconds (`--secs <n>`).
    ///
    /// Exists so the orchestrator's own unit tests and local smoke runs do
    /// not have to wait the full budget; CI never passes it.
    pub secs: Option<u64>,
    /// Reproduce an earlier run by fixing its PRNG seed (`--seed <n>`).
    ///
    /// Unset (the default, including in `ci`) draws a fresh per-harness seed
    /// each run, so consecutive soaks explore new inputs.
    /// Setting it replays the exact stream the orchestrator logged for a
    /// reported crash.
    pub seed: Option<u64>,
}

/// Parse `fuzz` arguments. `--quick` is the default when neither budget flag
/// is given.
///
/// # Errors
/// Returns a usage error for an unknown flag, a missing flag value, a
/// non-numeric `--secs`, or a `--quick`/`--soak` conflict.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut mode: Option<Mode> = None;
    let mut only: Option<String> = None;
    let mut list = false;
    let mut secs: Option<u64> = None;
    let mut seed: Option<u64> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.to_str() else {
            return Err(format!(
                "fuzz: argument {} is not valid UTF-8",
                arg.display()
            ));
        };
        match flag {
            "--once" => set_mode(&mut mode, Mode::Once)?,
            "--quick" => set_mode(&mut mode, Mode::Quick)?,
            "--soak" => set_mode(&mut mode, Mode::Soak)?,
            "--list" => list = true,
            "--target" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fuzz: `--target` requires a harness name".to_string())?;
                let name = value
                    .to_str()
                    .ok_or_else(|| "fuzz: `--target` value is not valid UTF-8".to_string())?;
                only = Some(name.to_string());
            }
            "--secs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fuzz: `--secs` requires a number".to_string())?;
                let parsed = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        format!("fuzz: `--secs` expects a u64, got {}", value.display())
                    })?;
                secs = Some(parsed);
            }
            "--seed" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "fuzz: `--seed` requires a number".to_string())?;
                let parsed = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        format!("fuzz: `--seed` expects a u64, got {}", value.display())
                    })?;
                seed = Some(parsed);
            }
            other => {
                return Err(format!(
                    "fuzz: unexpected argument {other:?}; usage: \
                     cargo xtask fuzz [--once | --quick | --soak] [--target NAME] [--secs N] \
                     [--seed N] [--list]"
                ));
            }
        }
    }

    Ok(Options {
        mode: mode.unwrap_or(Mode::Quick),
        only,
        list,
        secs,
        seed,
    })
}

fn set_mode(slot: &mut Option<Mode>, mode: Mode) -> Result<(), String> {
    match slot {
        Some(existing) if *existing != mode => {
            Err("fuzz: `--once`, `--quick`, and `--soak` are mutually exclusive".to_string())
        }
        _ => {
            *slot = Some(mode);
            Ok(())
        }
    }
}

/// Resolve the harnesses an [`Options`] selects, preserving registry order.
///
/// # Errors
/// Returns an error if `--target` names a harness that is not registered.
pub fn selected(opts: &Options) -> Result<Vec<Target>, String> {
    let Some(name) = opts.only.as_deref() else {
        return Ok(TARGETS.to_vec());
    };
    match TARGETS.iter().find(|t| t.test == name) {
        Some(t) => Ok(vec![*t]),
        None => Err(format!(
            "fuzz: unknown target `{name}`; known targets: {}",
            TARGETS
                .iter()
                .map(|t| t.test)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Run the selected harnesses for their budget.
pub fn run(ctx: &Context, opts: &Options) -> Result<(), String> {
    if opts.list {
        for t in TARGETS {
            println!("{:<24} {}  [{}]", t.test, t.description, t.package);
        }
        return Ok(());
    }

    // An explicit `--secs N` always wins; otherwise the mode supplies the
    // budget (`--once` is `None`: a single smoke iteration, no budget env).
    let budget: Option<Duration> = match opts.secs {
        Some(s) if s > 0 => Some(Duration::from_secs(s)),
        Some(_) => None,
        None => opts.mode.budget(),
    };
    let targets = selected(opts)?;
    // Each harness is an independent, budget-bounded host process, so the
    // registry runs concurrently rather than paying the sum of every
    // harness's budget. The shared runner caps concurrency at the host's
    // parallelism and fails closed (`commands::parallel`).
    let jobs: Vec<Job> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| job_for(ctx, t, budget, opts.seed, i))
        .collect();
    let concurrency = parallel::default_concurrency(jobs.len());
    parallel::run(jobs, concurrency)
}

/// Build the `cargo test` [`Job`] that runs one fuzz harness.
///
/// `budget` is the wall-clock soak budget exported to the harness (`None`
/// runs the single fixed-iteration smoke sweep, as `--once` does). `seed` and
/// `index` pick the per-job PRNG seed (`None` draws a fresh entropy seed each
/// call, so repeated runs explore new inputs; `Some(base)` reproduces a logged
/// stream). The chosen seed is baked into the job label so it reaches the log.
///
/// Shared by [`run`] and the long-CI flake hunt ([`super::ci_long`]) so both
/// build a harness job the one way.
pub(crate) fn job_for(
    ctx: &Context,
    target: &Target,
    budget: Option<Duration>,
    seed: Option<u64>,
    index: usize,
) -> Job {
    let mut cmd = ctx.cargo();
    // `--test <name>` runs exactly that integration harness; `--exact`
    // is unnecessary because the test binary contains only fuzz fns.
    cmd.args([
        "test",
        "-p",
        target.package,
        "--test",
        target.test,
        "--locked",
        "--",
        "--nocapture",
    ]);
    // A budget turns the harness into a soak loop; without one the harness
    // runs its single smoke iteration.
    if let Some(budget) = budget {
        cmd.env(
            tairix_fuzzseed::FUZZ_BUDGET_ENV,
            budget.as_secs().to_string(),
        );
    }
    // Each harness reads this seed instead of its built-in constant, so a fresh
    // seed (the default) makes every run explore new inputs while an explicit
    // seed reproduces a logged crash exactly.
    let job_seed = seed::job_seed(seed, index);
    cmd.env(seed::FUZZ_SEED_ENV, job_seed.to_string());
    let budget_desc = match budget {
        Some(b) => format!("{} s", b.as_secs()),
        None => "1 iteration".to_string(),
    };
    let label = format!("fuzz {} ({budget_desc}, seed {job_seed})", target.test);
    Job::new(label, cmd).with_soak_budget(budget)
}

#[cfg(test)]
mod tests {
    use super::{parse, selected, Mode, TARGETS};
    use std::ffi::OsString;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_to_quick_running_every_target() {
        let opts = parse(&argv(&[])).expect("empty args parse");
        assert_eq!(opts.mode, Mode::Quick);
        assert!(opts.only.is_none());
        assert!(!opts.list);
        assert_eq!(selected(&opts).expect("all targets").len(), TARGETS.len());
    }

    #[test]
    fn once_flag_selects_a_single_iteration_with_no_budget() {
        let opts = parse(&argv(&["--once"])).expect("once parses");
        assert_eq!(opts.mode, Mode::Once);
        // No wall-clock budget: the harness runs its single smoke iteration.
        assert!(Mode::Once.budget().is_none());
    }

    #[test]
    fn quick_budget_meets_the_five_second_floor() {
        // the charter mandates ≥ 5 s per harness for the budgeted soak step.
        assert!(Mode::Quick.budget().expect("quick is budgeted").as_secs() >= 5);
    }

    #[test]
    fn soak_budget_meets_the_twenty_four_hour_floor() {
        // the charter mandates ≥ 24 h per harness for the nightly soak.
        assert!(Mode::Soak.budget().expect("soak is budgeted").as_secs() >= 24 * 60 * 60);
    }

    #[test]
    fn soak_flag_selects_the_soak_budget() {
        let opts = parse(&argv(&["--soak"])).expect("soak parses");
        assert_eq!(opts.mode, Mode::Soak);
    }

    #[test]
    fn target_filter_selects_one_known_harness() {
        let opts = parse(&argv(&["--target", "fuzz_decode"])).expect("target parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].test, "fuzz_decode");
    }

    #[test]
    fn unknown_target_fails_closed() {
        let opts = parse(&argv(&["--target", "no_such_harness"])).expect("flag parses");
        assert!(selected(&opts).is_err());
    }

    #[test]
    fn conflicting_budget_flags_are_rejected() {
        assert!(parse(&argv(&["--quick", "--soak"])).is_err());
    }

    #[test]
    fn repeating_the_same_budget_flag_is_accepted() {
        let opts = parse(&argv(&["--quick", "--quick"])).expect("idempotent flag");
        assert_eq!(opts.mode, Mode::Quick);
    }

    #[test]
    fn secs_override_parses_and_wins() {
        let opts = parse(&argv(&["--secs", "3"])).expect("secs parses");
        assert_eq!(opts.secs, Some(3));
    }

    #[test]
    fn secs_requires_a_number() {
        assert!(parse(&argv(&["--secs", "soon"])).is_err());
        assert!(parse(&argv(&["--secs"])).is_err());
    }

    #[test]
    fn seed_defaults_to_none_so_each_run_is_fresh() {
        // No `--seed` means the orchestrator draws a fresh seed per run, so
        // consecutive soaks explore new inputs.
        let opts = parse(&argv(&[])).expect("empty args parse");
        assert_eq!(opts.seed, None);
    }

    #[test]
    fn seed_override_parses() {
        let opts = parse(&argv(&["--seed", "12345"])).expect("seed parses");
        assert_eq!(opts.seed, Some(12345));
    }

    #[test]
    fn seed_requires_a_number() {
        assert!(parse(&argv(&["--seed", "later"])).is_err());
        assert!(parse(&argv(&["--seed"])).is_err());
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse(&argv(&["--turbo"])).is_err());
    }

    #[test]
    fn list_flag_is_parsed() {
        let opts = parse(&argv(&["--list"])).expect("list parses");
        assert!(opts.list);
    }

    #[test]
    fn every_registered_target_has_a_unique_test_name() {
        for (i, a) in TARGETS.iter().enumerate() {
            for b in &TARGETS[i + 1..] {
                assert_ne!(a.test, b.test, "duplicate fuzz target name");
            }
        }
    }

    #[test]
    fn net_stack_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_net_stack"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-net");
    }

    #[test]
    fn arxfs_mount_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_mount"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-drv-fs-arxfs");
    }

    #[test]
    fn compress_decode_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_compress"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-compress");
    }

    #[test]
    fn svg_decode_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_svg"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-svg");
    }

    #[test]
    fn virtqueue_fuzz_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_virtqueue"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-virtio");
    }

    #[test]
    fn vt_parser_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_vt"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-vt");
    }

    #[test]
    fn fdt_reader_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_fdt"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-fdt");
    }

    #[test]
    fn curses_input_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_curses_input"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-curses");
    }

    #[test]
    fn glob_matcher_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_glob"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-glob");
    }

    #[test]
    fn path_parser_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_path"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-path");
    }

    #[test]
    fn devids_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_devids"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-devids");
    }

    #[test]
    fn binfmt_harnesses_are_registered() {
        for name in ["fuzz_rxe", "fuzz_elf", "fuzz_wasm"] {
            let opts = parse(&argv(&["--target", name])).expect("flag parses");
            let chosen = selected(&opts).expect("known target");
            assert_eq!(chosen.len(), 1);
            assert_eq!(chosen[0].package, "tairix-binfmt");
        }
    }

    #[test]
    fn supervisor_repl_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_repl"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-supervisor");
    }

    #[test]
    fn ipc_port_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_port"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-kernel-ipc");
    }

    #[test]
    fn program_library_store_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_proglib"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-proglib");
    }

    #[test]
    fn pinboard_settings_store_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_wallpaper_settings"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-wallpaper");
    }

    #[test]
    fn raster_image_harness_is_registered() {
        let opts = parse(&argv(&["--target", "fuzz_image"])).expect("flag parses");
        let chosen = selected(&opts).expect("known target");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-image");
    }

    /// Collects the harness names (`fuzz_<name>` integration-test files) a
    /// crate publishes under its own `tests/` directory.
    fn harnesses_below(dir: &std::path::Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let in_tests_dir = dir.file_name().is_some_and(|name| name == "tests");
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                if name != "target" {
                    harnesses_below(&path, found);
                }
            } else if in_tests_dir {
                if let Some(stem) = name.strip_suffix(".rs") {
                    if stem.starts_with("fuzz_") {
                        found.push(stem.to_owned());
                    }
                }
            }
        }
    }

    /// A harness that exists on disk but is absent from the registry is never
    /// run by the gate, so its subject is unfuzzed however green CI looks.
    #[test]
    fn every_harness_in_the_tree_is_registered() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("xtask sits two levels below the workspace root");
        let mut found = Vec::new();
        for area in ["kernel", "drivers", "lib", "userland", "tools", "tests"] {
            harnesses_below(&root.join(area), &mut found);
        }
        assert!(
            !found.is_empty(),
            "walked {} but found no harnesses at all",
            root.display()
        );
        let unregistered: Vec<&String> = found
            .iter()
            .filter(|name| !TARGETS.iter().any(|t| t.test == name.as_str()))
            .collect();
        assert!(
            unregistered.is_empty(),
            "fuzz harnesses missing from the registry: {unregistered:?}"
        );
    }

    #[test]
    fn registry_covers_the_burn_down_endpoints() {
        // wire decoders, dispatcher, lib/net protocol parsers, IPC port.
        for required in ["fuzz_decode", "fuzz_args", "fuzz_net_icmp", "fuzz_port"] {
            assert!(
                TARGETS.iter().any(|t| t.test == required),
                "missing required fuzz harness {required}"
            );
        }
    }
}
