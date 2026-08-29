//! The parsed shape of a `sysinfo` command line.

use crate::error::SysinfoError;

/// One thing the `sysinfo` tool can do.
///
/// Each variant maps to exactly one `sysinfo-v1` query (or to printing the
/// usage banner). There is intentionally no free-form "raw query id" escape
/// hatch: the tool only ever issues queries the frozen registry defines.
///
/// [`Show`](Self::Show) and [`Describe`](Self::Describe) carry a *resource
/// reference*, which is not an escape hatch from that rule: a reference is a
/// name in the `info:`/`state:`/`stats:` namespaces, not a query id. It is
/// mapped onto a query by `lib/procinfo`'s resolver, whose match arms are the
/// closed set of selectors the registry catalogues, and which fails closed on
/// anything outside it. No spelling of a reference can therefore reach a
/// query this tool could not already issue — the invariant holds, by
/// construction rather than by a length check on the string.
///
/// The lifetime is the argument vector's: the reference is borrowed from the
/// `argv` slice [`parse`] is handed rather than copied, so the parsed command
/// stays [`Copy`] and allocation-free.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command<'a> {
    /// List processes. `all` selects the system-wide view
    /// (`SysinfoQueryId::GLOBAL_PROCESS_LIST`, which the service gates on
    /// `CAP_SYSINFO_GLOBAL`); otherwise the caller's own processes
    /// (`SELF_PROCESS_LIST`, ungated).
    Processes {
        /// Request the global process list rather than the caller's own.
        all: bool,
    },
    /// Read kernel memory statistics (`KERNEL_MEMORY_STATS`).
    Memory,
    /// Read the detected hardware tree (`HARDWARE_TREE`).
    Hardware,
    /// Read the machine identity (`SYSTEM_IDENTITY`).
    Identity,
    /// Read system uptime and boot time (`UPTIME`).
    Uptime,
    /// Read the caller's own effective resource limits and live usage
    /// (`RESOURCE_LIMITS`).
    Limits,
    /// List the seats — each display's owner, lease generation, and
    /// foreground console (`SEAT_LIST`, which the service gates on
    /// `CAP_SYSINFO_HW`).
    Seats,
    /// Read the live memory-pressure gauge (`MEMORY_PRESSURE`, which the
    /// service gates on `CAP_SYSINFO_KERNEL`).
    Pressure,
    /// Read the reclaimable-cache ledger, one row per class
    /// (`RECLAIM_STATS`, gated on `CAP_SYSINFO_KERNEL`).
    Reclaim,
    /// Read the `ramzip` compressed-tier counters (`RAMZIP_STATS`, gated
    /// on `CAP_SYSINFO_KERNEL`).
    Ramzip,
    /// Read the per-CPU scheduler load figures, one row per CPU
    /// (`CPU_LOAD`, gated on `CAP_SYSINFO_KERNEL`).
    CpuLoad,
    /// Read the per-CPU processor information — vendor/model, performance
    /// class, ISA-extension flags, identity, and the live core-clock and
    /// reference frequencies — the `/proc/cpuinfo`-class report
    /// (`CPU_INFO`, ungated).
    CpuInfo,
    /// List the kernel IRQ table, one row per bound interrupt line — the
    /// line id, the owning driver task, the interrupt count since boot, and
    /// whether the line is quarantined (`IRQ_LIST`, which the service gates
    /// on `CAP_SYSINFO_HW`).
    Irqs,
    /// Read what each desktop session's composited frames have cost — the
    /// pixels recomposed, what resolving them blended, the worst single
    /// frame, and the furniture cache's hit tally
    /// (`DESKTOP_FRAME_STATS`, which the service gates on
    /// `CAP_SYSINFO_GLOBAL`: a row names another principal — the session
    /// process — and its work).
    Frames,
    /// List per-volume storage I/O health, one row per fault-aware
    /// block-backed volume — its durable id, the serving block-service
    /// endpoint, its current availability, and the folded outcome counters
    /// (`VOLUME_IO_HEALTH`, which the service gates on `CAP_SYSINFO_KERNEL`).
    Storage,
    /// List the composed RAID arrays and the devices the array composer
    /// holds — each array's level, health, member tally and rebuild/scrub
    /// progress, then each device's array affiliation, slot, and disposition
    /// (`RAID_ARRAYS` and `RAID_MEMBERS`, which the service gates on
    /// `CAP_SYSINFO_HW`: the composition of the machine's storage is read
    /// under the same authority as the hardware tree, not the kernel-state
    /// authority the per-volume `storage` counters need).
    Raid,
    /// Read one `info:`/`state:`/`stats:` resource reference and print its
    /// value (`plans/ALIAS.md` §15.4 `show`).
    ///
    /// The reference is resolved through `lib/procinfo`'s userspace resolver
    /// over the System Information API — the one place those namespaces are
    /// resolved — so this is not a second reader and cannot bypass the
    /// broker's per-principal scoping. `cat <ref>` and `cat < <ref>` are the
    /// other two spellings of the same read; all three resolve through this
    /// resolver and render through the same `display_value`, so their bytes
    /// match exactly.
    Show {
        /// The resource reference to read, as spelled on the command line.
        reference: &'a str,
    },
    /// Print the response *envelope* for one resource reference — its
    /// producer, the authorization it was served under, and the payload's own
    /// metadata (`plans/ALIAS.md` §14.5 `describe`).
    ///
    /// The value itself is [`Show`](Self::Show)'s job; this answers "what is
    /// this figure, and may I trust it": for a metric its kind, unit, reset
    /// behaviour, and sampling window; for a fact its type and sensitivity.
    Describe {
        /// The resource reference to describe, as spelled on the command line.
        reference: &'a str,
    },
    /// Render `sysinfo`'s own short help (`help`/`-h`/`-?`/`--help`): the
    /// `NAME`, `SYNOPSIS`, and compact `OPTIONS` of its Help document,
    /// through the same engine as any other command's short help
    /// (plans/APPS.md §4). The default when no arguments are given.
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is a single subcommand optionally followed by flags. It
/// **fails closed**: an unknown subcommand, an unknown flag, or a stray
/// trailing argument is a [`SysinfoError::Usage`] rather than a silently
/// ignored token.
///
/// Recognised forms:
///
/// | Subcommand            | Meaning                          |
/// |-----------------------|----------------------------------|
/// | (none)                | [`Command::Help`]                |
/// | `help`, `-h`, `-?`, `--help` | [`Command::Help`]         |
/// | `processes`, `ps`     | [`Command::Processes`] (`--all`/`-a` for the global view) |
/// | `memory`, `mem`       | [`Command::Memory`]              |
/// | `hardware`, `hw`      | [`Command::Hardware`]            |
/// | `identity`, `id`      | [`Command::Identity`]            |
/// | `uptime`              | [`Command::Uptime`]              |
/// | `limits`, `rlimits`   | [`Command::Limits`]              |
/// | `seats`               | [`Command::Seats`]               |
/// | `pressure`            | [`Command::Pressure`]            |
/// | `reclaim`             | [`Command::Reclaim`]             |
/// | `ramzip`              | [`Command::Ramzip`]              |
/// | `cpu`                 | [`Command::CpuLoad`]             |
/// | `cpuinfo`             | [`Command::CpuInfo`]             |
/// | `irq`, `irqs`         | [`Command::Irqs`]                |
/// | `frames`              | [`Command::Frames`]              |
/// | `storage`, `io`       | [`Command::Storage`]             |
/// | `raid`, `arrays`      | [`Command::Raid`]                |
/// | `show <ref>`          | [`Command::Show`]                |
/// | `describe <ref>`      | [`Command::Describe`]            |
///
/// `show` and `describe` take exactly one operand — a resource reference —
/// and no switches. The operand is not validated here beyond being present:
/// its grammar belongs to the shared reference parser, and its selector to
/// the resolver, so this parser neither embeds a second reference grammar nor
/// pre-judges what the resolver serves.
///
/// # Errors
///
/// [`SysinfoError::Usage`] for any input outside the grammar above, including
/// `show`/`describe` with no operand or with more than one.
pub fn parse<'a>(args: &[&'a str]) -> Result<Command<'a>, SysinfoError> {
    let Some((&subcommand, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    match subcommand {
        "help" | "-h" | "-?" | "--help" => no_more(rest).map(|()| Command::Help),
        "processes" | "ps" => parse_processes(rest),
        "memory" | "mem" => no_more(rest).map(|()| Command::Memory),
        "hardware" | "hw" => no_more(rest).map(|()| Command::Hardware),
        "identity" | "id" => no_more(rest).map(|()| Command::Identity),
        "uptime" => no_more(rest).map(|()| Command::Uptime),
        "limits" | "rlimits" => no_more(rest).map(|()| Command::Limits),
        "seats" => no_more(rest).map(|()| Command::Seats),
        "pressure" => no_more(rest).map(|()| Command::Pressure),
        "reclaim" => no_more(rest).map(|()| Command::Reclaim),
        "ramzip" => no_more(rest).map(|()| Command::Ramzip),
        "cpu" => no_more(rest).map(|()| Command::CpuLoad),
        "cpuinfo" => no_more(rest).map(|()| Command::CpuInfo),
        "irq" | "irqs" => no_more(rest).map(|()| Command::Irqs),
        "frames" => no_more(rest).map(|()| Command::Frames),
        "storage" | "io" => no_more(rest).map(|()| Command::Storage),
        "raid" | "arrays" => no_more(rest).map(|()| Command::Raid),
        "show" => one_operand(rest).map(|reference| Command::Show { reference }),
        "describe" => one_operand(rest).map(|reference| Command::Describe { reference }),
        _ => Err(SysinfoError::Usage),
    }
}

/// Take the single operand a subcommand requires, borrowed from `argv`.
///
/// Fails closed on none and on more than one: a second word is never
/// silently ignored, and an empty operand is not a reference.
fn one_operand<'a>(args: &[&'a str]) -> Result<&'a str, SysinfoError> {
    match args {
        [only] if !only.is_empty() => Ok(only),
        _ => Err(SysinfoError::Usage),
    }
}

/// Parse the flags accepted by the `processes` subcommand.
///
/// The returned command borrows nothing, so its lifetime is free to be the
/// caller's.
fn parse_processes(args: &[&str]) -> Result<Command<'static>, SysinfoError> {
    let mut all = false;
    for &arg in args {
        match arg {
            "--all" | "-a" => all = true,
            _ => return Err(SysinfoError::Usage),
        }
    }
    Ok(Command::Processes { all })
}

/// Reject any trailing argument for a subcommand that takes none.
fn no_more(args: &[&str]) -> Result<(), SysinfoError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(SysinfoError::Usage)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::SysinfoError;

    #[test]
    fn no_arguments_is_help() {
        assert_eq!(parse(&[]), Ok(Command::Help));
    }

    #[test]
    fn help_aliases_parse() {
        for arg in ["help", "-h", "-?", "--help"] {
            assert_eq!(parse(&[arg]), Ok(Command::Help));
        }
    }

    #[test]
    fn processes_self_and_global() {
        assert_eq!(parse(&["processes"]), Ok(Command::Processes { all: false }));
        assert_eq!(parse(&["ps"]), Ok(Command::Processes { all: false }));
        assert_eq!(
            parse(&["ps", "--all"]),
            Ok(Command::Processes { all: true })
        );
        assert_eq!(parse(&["ps", "-a"]), Ok(Command::Processes { all: true }));
    }

    #[test]
    fn scalar_subcommands_and_aliases() {
        assert_eq!(parse(&["memory"]), Ok(Command::Memory));
        assert_eq!(parse(&["mem"]), Ok(Command::Memory));
        assert_eq!(parse(&["hardware"]), Ok(Command::Hardware));
        assert_eq!(parse(&["hw"]), Ok(Command::Hardware));
        assert_eq!(parse(&["identity"]), Ok(Command::Identity));
        assert_eq!(parse(&["id"]), Ok(Command::Identity));
        assert_eq!(parse(&["uptime"]), Ok(Command::Uptime));
        assert_eq!(parse(&["limits"]), Ok(Command::Limits));
        assert_eq!(parse(&["rlimits"]), Ok(Command::Limits));
        assert_eq!(parse(&["seats"]), Ok(Command::Seats));
        assert_eq!(parse(&["pressure"]), Ok(Command::Pressure));
        assert_eq!(parse(&["reclaim"]), Ok(Command::Reclaim));
        assert_eq!(parse(&["ramzip"]), Ok(Command::Ramzip));
        assert_eq!(parse(&["cpu"]), Ok(Command::CpuLoad));
        assert_eq!(parse(&["cpuinfo"]), Ok(Command::CpuInfo));
        assert_eq!(parse(&["irq"]), Ok(Command::Irqs));
        assert_eq!(parse(&["irqs"]), Ok(Command::Irqs));
        assert_eq!(parse(&["frames"]), Ok(Command::Frames));
        assert_eq!(parse(&["storage"]), Ok(Command::Storage));
        assert_eq!(parse(&["io"]), Ok(Command::Storage));
        assert_eq!(parse(&["raid"]), Ok(Command::Raid));
        assert_eq!(parse(&["arrays"]), Ok(Command::Raid));
    }

    #[test]
    fn show_and_describe_take_one_reference() {
        assert_eq!(
            parse(&["show", "info:system/hostname"]),
            Ok(Command::Show {
                reference: "info:system/hostname"
            })
        );
        assert_eq!(
            parse(&["describe", "stats:net/wan/rx.pps?window=1s"]),
            Ok(Command::Describe {
                reference: "stats:net/wan/rx.pps?window=1s"
            })
        );
    }

    /// The operand is required, singular, and non-empty: a missing, doubled,
    /// or blank reference is a usage error rather than a guess.
    #[test]
    fn show_and_describe_fail_closed_without_exactly_one_reference() {
        for args in [
            alloc::vec!["show"],
            alloc::vec!["describe"],
            alloc::vec!["show", ""],
            alloc::vec!["show", "sys:null", "sys:random"],
            alloc::vec!["describe", "stats:uptime", "--verbose"],
        ] {
            assert_eq!(parse(&args), Err(SysinfoError::Usage), "{args:?}");
        }
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert_eq!(parse(&["frobnicate"]), Err(SysinfoError::Usage));
    }

    #[test]
    fn unknown_flag_is_usage() {
        assert_eq!(parse(&["ps", "--everything"]), Err(SysinfoError::Usage));
    }

    #[test]
    fn trailing_argument_is_usage() {
        assert_eq!(parse(&["uptime", "now"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["memory", "extra"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["seats", "0"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["pressure", "now"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["cpu", "0"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["irq", "0"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["frames", "0"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["storage", "0"]), Err(SysinfoError::Usage));
        assert_eq!(parse(&["raid", "md0"]), Err(SysinfoError::Usage));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/sysinfo.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`--all, -a`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/sysinfo.md must document {switch}"
                );
            }
        }
    }
}
