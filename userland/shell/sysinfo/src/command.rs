//! The parsed shape of a `sysinfo` command line.

use crate::error::SysinfoError;

/// One thing the `sysinfo` tool can do.
///
/// Each variant maps to exactly one `sysinfo-v1` query (or to printing the
/// usage banner). There is intentionally no free-form "raw query id" escape
/// hatch: the tool only ever issues queries the frozen registry defines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
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
///
/// # Errors
///
/// [`SysinfoError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, SysinfoError> {
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
        _ => Err(SysinfoError::Usage),
    }
}

/// Parse the flags accepted by the `processes` subcommand.
fn parse_processes(args: &[&str]) -> Result<Command, SysinfoError> {
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
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
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
