//! Read-only informational commands.
//!
//! Each renders state an existing kernel subsystem already computes, through
//! the [`SupervisorHost`](crate::SupervisorHost) seam, and returns
//! `Flow::Stay`. None writes to storage or exposes an arbitrary
//! physical-address read.

use crate::commands::{arg_str, missing_arg};
use crate::dispatch::{Flow, Session};

/// `version` — kernel version, build identity, target, and ABI version.
pub fn cmd_version(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.version(session.out);
    Flow::Stay
}

/// `mem [map]` — memory summary, or the boot memory map with `mem map`.
pub fn cmd_mem(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    match args.get(1) {
        Some(sub) if arg_str(sub) == Some("map") => session.host.memory_map(session.out),
        Some(other) => {
            session.out.write_str("mem: unknown subcommand: ");
            session.out.write_bytes(other);
            session.out.newline();
        }
        None => session.host.memory(session.out),
    }
    Flow::Stay
}

/// `cpu` — CPU / core count and detected features.
pub fn cmd_cpu(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.cpu(session.out);
    Flow::Stay
}

/// `hw` / `lsdev` — dump the discovered hardware tree.
pub fn cmd_hw(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.hardware(session.out);
    Flow::Stay
}

/// `disk` — list attached block devices and geometry.
pub fn cmd_disk(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.disks(session.out);
    Flow::Stay
}

/// `partitions <device>` — parse and show a device's partition table.
pub fn cmd_partitions(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    let Some(device) = args.get(1).and_then(|token| arg_str(token)) else {
        missing_arg(session.out, "partitions <device>");
        return Flow::Stay;
    };
    session.host.partitions(device, session.out);
    Flow::Stay
}

/// `arxfs` — root volume descriptor / label / status, without unlocking.
pub fn cmd_arxfs(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.arxfs_status(session.out);
    Flow::Stay
}

/// `ls [path]` — list a directory (pre-mount: the `/System` volume).
pub fn cmd_ls(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    let path = args.get(1).and_then(|token| arg_str(token));
    session.host.list(path, session.out);
    Flow::Stay
}

/// `uptime` — monotonic time since boot.
pub fn cmd_uptime(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.uptime(session.out);
    Flow::Stay
}

/// `date` — wall-clock date and time.
pub fn cmd_date(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    session.host.date(session.out);
    Flow::Stay
}

/// `echo [args...]` — print the arguments separated by single spaces.
pub fn cmd_echo(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    for (i, token) in args.iter().skip(1).enumerate() {
        if i > 0 {
            session.out.write_bytes(b" ");
        }
        session.out.write_bytes(token);
    }
    session.out.newline();
    Flow::Stay
}

/// `clear` / `cls` — clear the screen and home the cursor.
pub fn cmd_clear(_args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    // Erase the whole display, then move the cursor to the top-left — the
    // standard ANSI clear the shared `lib/vt` vocabulary also emits.
    session.out.write_bytes(b"\x1b[2J\x1b[H");
    Flow::Stay
}

#[cfg(test)]
mod tests {
    use crate::commands::test_support::MockSession;
    use crate::dispatch::{dispatch, Flow};

    #[test]
    fn version_renders_from_the_host() {
        let mut s = MockSession::new(&[]);
        assert_eq!(dispatch(b"version", &mut s.session()), Flow::Stay);
        assert!(s.output_contains("TAIRiX"));
    }

    #[test]
    fn mem_and_mem_map_call_distinct_host_methods() {
        let mut s = MockSession::new(&[]);
        dispatch(b"mem", &mut s.session());
        assert!(s.output_contains("usable RAM"));
        let mut s2 = MockSession::new(&[]);
        dispatch(b"mem map", &mut s2.session());
        assert!(s2.output_contains("memory map"));
    }

    #[test]
    fn partitions_without_a_device_reports_usage() {
        let mut s = MockSession::new(&[]);
        dispatch(b"partitions", &mut s.session());
        assert!(s.output_contains("usage"));
    }

    #[test]
    fn partitions_with_a_device_queries_the_host() {
        let mut s = MockSession::new(&[]);
        dispatch(b"partitions disk0", &mut s.session());
        assert!(s.output_contains("disk0"));
    }

    #[test]
    fn echo_prints_its_arguments() {
        let mut s = MockSession::new(&[]);
        dispatch(b"echo hello  world", &mut s.session());
        assert!(s.output_contains("hello world"));
    }

    #[test]
    fn clear_emits_the_ansi_clear_sequence() {
        let mut s = MockSession::new(&[]);
        dispatch(b"clear", &mut s.session());
        assert_eq!(s.output_bytes(), b"\x1b[2J\x1b[H");
    }

    #[test]
    fn ls_defaults_to_the_system_volume() {
        let mut s = MockSession::new(&[]);
        dispatch(b"ls", &mut s.session());
        assert!(s.output_contains("System"));
    }
}
