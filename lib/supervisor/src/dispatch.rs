//! The Supervisor command table, line tokeniser, and dispatcher.
//!
//! The dispatcher is a complete, `&'static` table of built-in commands — not
//! the minimum for one command. Each [`Command`] carries its name, any
//! aliases, a one-line summary for the `help` table, a longer per-command
//! help body, and a handler. Dispatch tokenises the line, matches the first
//! token against the table (ASCII case-insensitively, so `Help` and `help`
//! both work), and runs the handler; an empty line is a no-op and an unknown
//! command is a fail-soft message, never a panic.

use crate::commands;
use crate::{Report, SupInput, SupervisorExit, SupervisorHost, MAX_TOKENS};

/// The mutable environment a command handler runs against: the output sink,
/// the keyboard input, and the data/control host.
pub struct Session<'a> {
    /// Where command output is rendered.
    pub out: &'a mut dyn Report,
    /// The keyboard byte source (for `mount`'s passphrase read and the
    /// long-running commands' `ESC` abort poll).
    pub input: &'a mut dyn SupInput,
    /// The data and control seam over the kernel's existing subsystems.
    pub host: &'a mut dyn SupervisorHost,
}

/// What a handler decided the REPL should do next.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Flow {
    /// Stay in the REPL and read the next command.
    Stay,
    /// Leave the REPL with the given outcome (the boot path acts on it).
    Exit(SupervisorExit),
}

/// One built-in Supervisor command.
pub struct Command {
    /// The primary command word (lower-case).
    pub name: &'static str,
    /// Alternative words that invoke the same handler.
    pub aliases: &'static [&'static str],
    /// One-line description shown in the `help` summary table.
    pub summary: &'static str,
    /// The longer help body shown by `help <name>`.
    pub help: &'static str,
    /// The handler; `args` are the whitespace tokens of the line, `args[0]`
    /// being the command word itself.
    pub handler: fn(args: &[&[u8]], session: &mut Session<'_>) -> Flow,
}

impl Command {
    /// Whether `word` names this command (its name or one of its aliases),
    /// compared ASCII case-insensitively.
    #[must_use]
    pub fn matches(&self, word: &[u8]) -> bool {
        if eq_ignore_ascii_case(word, self.name.as_bytes()) {
            return true;
        }
        self.aliases
            .iter()
            .any(|alias| eq_ignore_ascii_case(word, alias.as_bytes()))
    }
}

/// ASCII case-insensitive byte-slice equality (no allocation, no locale).
fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// The complete built-in command set, grouped control / info / diagnostics.
///
/// The order here is the order the `help` summary lists them, so control
/// commands lead and the diagnostics follow.
pub static COMMANDS: &[Command] = &[
    Command {
        name: "help",
        aliases: &["?"],
        summary: "list commands, or show help for one (help <cmd>)",
        help: "help [command]\n\n  With no argument, list every command with a one-line summary.\n  With a command name, show that command's detailed help.",
        handler: cmd_help,
    },
    Command {
        name: "continue",
        aliases: &["boot"],
        summary: "leave the Supervisor and resume the normal boot",
        help: "continue (alias: boot)\n\n  Leave the Supervisor and resume the normal boot: the passphrase\n  prompt is redrawn and boot carries on as though the Supervisor had\n  never been entered.",
        handler: commands::control::cmd_continue,
    },
    Command {
        name: "mount",
        aliases: &[],
        summary: "unlock and mount the root now (prompts for the passphrase)",
        help: "mount\n\n  Prompt for the ARXFS passphrase and perform the real root unlock\n  now. On success the boot continues without a second prompt. A wrong\n  passphrase or a structural failure is reported; nothing is bypassed.",
        handler: commands::control::cmd_mount,
    },
    Command {
        name: "reboot",
        aliases: &[],
        summary: "restart the machine",
        help: "reboot\n\n  Cleanly restart the machine through the platform reset primitive.",
        handler: commands::control::cmd_reboot,
    },
    Command {
        name: "poweroff",
        aliases: &["halt"],
        summary: "power off / halt the machine",
        help: "poweroff (alias: halt)\n\n  Power the machine off where the platform supports it; otherwise\n  report that power-off is unavailable and stay in the Supervisor.",
        handler: commands::control::cmd_poweroff,
    },
    Command {
        name: "version",
        aliases: &["ver"],
        summary: "kernel version, build, target, and ABI version",
        help: "version (alias: ver)\n\n  Show the kernel version, build identity, target architecture, and\n  ABI version.",
        handler: commands::info::cmd_version,
    },
    Command {
        name: "mem",
        aliases: &[],
        summary: "memory summary (mem), or the boot memory map (mem map)",
        help: "mem [map]\n\n  With no argument, show installed/usable RAM, the kernel heap size,\n  and the memory-pressure band. `mem map` prints the boot memory map\n  (usable and reserved regions).",
        handler: commands::info::cmd_mem,
    },
    Command {
        name: "cpu",
        aliases: &[],
        summary: "CPU / core count and detected features",
        help: "cpu\n\n  Show the CPU / core count and the detected CPU features.",
        handler: commands::info::cmd_cpu,
    },
    Command {
        name: "hw",
        aliases: &["lsdev"],
        summary: "dump the discovered hardware tree",
        help: "hw (alias: lsdev)\n\n  Dump the discovered hardware tree: each node's class and bind keys.\n  Useful for finding why a disk or keyboard did not appear.",
        handler: commands::info::cmd_hw,
    },
    Command {
        name: "disk",
        aliases: &[],
        summary: "list attached block devices and their geometry",
        help: "disk\n\n  List the attached block devices and their geometry.",
        handler: commands::info::cmd_disk,
    },
    Command {
        name: "partitions",
        aliases: &["part"],
        summary: "show a device's partition table (partitions <dev>)",
        help: "partitions <device> (alias: part)\n\n  Parse and show the MBR/GPT partition table of the named device.",
        handler: commands::info::cmd_partitions,
    },
    Command {
        name: "arxfs",
        aliases: &[],
        summary: "root volume descriptor / label / status (no unlock)",
        help: "arxfs\n\n  Show the root volume's descriptor, label, and identity, and whether\n  it is present, is ARXFS, and is unlocked — without unlocking it.",
        handler: commands::info::cmd_arxfs,
    },
    Command {
        name: "ls",
        aliases: &[],
        summary: "list a directory (pre-mount: the /System volume)",
        help: "ls [path]\n\n  List a directory. Before the root is mounted the only readable\n  volume is /System, so ls is scoped there; after a mount it sees the\n  mounted tree. Read-only.",
        handler: commands::info::cmd_ls,
    },
    Command {
        name: "log",
        aliases: &[],
        summary: "tail the in-memory boot audit log (log [n])",
        help: "log [n]\n\n  Show the boot audit-log entries (the last n when a count is given).",
        handler: commands::diag::cmd_log,
    },
    Command {
        name: "panic-log",
        aliases: &["last"],
        summary: "show a previous boot's panic / lockup record, if any",
        help: "panic-log (alias: last)\n\n  Show a previous boot's recorded panic or CPU-lockup diagnostic, if\n  one was preserved; otherwise report that there is none.",
        handler: commands::diag::cmd_panic_log,
    },
    Command {
        name: "uptime",
        aliases: &[],
        summary: "monotonic time since boot",
        help: "uptime\n\n  Show the monotonic time elapsed since boot.",
        handler: commands::info::cmd_uptime,
    },
    Command {
        name: "date",
        aliases: &[],
        summary: "wall-clock date and time",
        help: "date\n\n  Show the wall-clock date and time.",
        handler: commands::info::cmd_date,
    },
    Command {
        name: "memtest",
        aliases: &[],
        summary: "thorough RAM test (memtest [passes] | memtest full); ESC aborts",
        help: "memtest [passes | full]\n\n  With a pass count (default 1), run the thorough, non-destructive\n  multi-pattern RAM test (walking ones/zeros, address-in-address, moving\n  inversions) over free memory the Supervisor owns. Progress is shown;\n  press ESC to abort. A fault is reported loudly.\n\n  `memtest full` (alias `memtest --takeover`) is the DESTRUCTIVE, one-way\n  whole-RAM test: it takes the whole machine over, overwrites ALL of RAM\n  (including the running kernel), and can only end in a reset. It demands\n  an explicit typed confirmation and never returns on a platform that\n  supports it.",
        handler: commands::diag::cmd_memtest,
    },
    Command {
        name: "test",
        aliases: &[],
        summary: "read-only disk surface scan (test disk <dev>); ESC aborts",
        help: "test disk <device>\n\n  Run a bounded, read-only surface scan of the named device, reporting\n  read errors or timeouts. Never writes. Press ESC to abort.",
        handler: commands::diag::cmd_test,
    },
    Command {
        name: "echo",
        aliases: &[],
        summary: "print the arguments",
        help: "echo [args...]\n\n  Print the arguments separated by single spaces, followed by a newline.",
        handler: commands::info::cmd_echo,
    },
    Command {
        name: "clear",
        aliases: &["cls"],
        summary: "clear the screen",
        help: "clear (alias: cls)\n\n  Clear the screen and move the cursor to the top-left.",
        handler: commands::info::cmd_clear,
    },
];

/// Split `line` into at most [`MAX_TOKENS`] whitespace-separated tokens,
/// writing them into `tokens` and returning how many were produced.
///
/// Whitespace is ASCII space or tab. Once the token array is full the
/// remainder of the line (from the start of the next token) is folded into a
/// final token, so a very wordy line never grows an unbounded table and no
/// input byte is silently dropped.
pub fn tokenize<'a>(line: &'a [u8], tokens: &mut [&'a [u8]; MAX_TOKENS]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < line.len() {
        while i < line.len() && is_space(line[i]) {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        if count + 1 == MAX_TOKENS {
            // Last slot: take the rest of the line verbatim so nothing is
            // lost, trimming a trailing run of spaces.
            let mut end = line.len();
            while end > i && is_space(line[end - 1]) {
                end -= 1;
            }
            tokens[count] = &line[i..end];
            count += 1;
            return count;
        }
        let start = i;
        while i < line.len() && !is_space(line[i]) {
            i += 1;
        }
        tokens[count] = &line[start..i];
        count += 1;
    }
    count
}

/// Whether `byte` is a token separator (ASCII space or tab).
const fn is_space(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// Tokenise and run one command `line`, returning the resulting [`Flow`].
///
/// An empty (all-whitespace) line is a no-op. An unknown command is reported
/// fail-soft with a pointer to `help`, never a panic.
pub fn dispatch(line: &[u8], session: &mut Session<'_>) -> Flow {
    let mut tokens: [&[u8]; MAX_TOKENS] = [&[]; MAX_TOKENS];
    let count = tokenize(line, &mut tokens);
    if count == 0 {
        return Flow::Stay;
    }
    let args = &tokens[..count];
    let word = args[0];
    for command in COMMANDS {
        if command.matches(word) {
            return (command.handler)(args, session);
        }
    }
    session.out.write_str("supervisor: unknown command: ");
    session.out.write_bytes(word);
    session.out.newline();
    session.out.line("Type `help` for the list of commands.");
    Flow::Stay
}

/// The `help` command: with no argument list every command and its summary;
/// with a command name show that command's detailed help.
fn cmd_help(args: &[&[u8]], session: &mut Session<'_>) -> Flow {
    if let Some(topic) = args.get(1) {
        for command in COMMANDS {
            if command.matches(topic) {
                session.out.line(command.help);
                return Flow::Stay;
            }
        }
        session.out.write_str("help: no such command: ");
        session.out.write_bytes(topic);
        session.out.newline();
        return Flow::Stay;
    }
    session.out.line("Supervisor commands:");
    for command in COMMANDS {
        session.out.write_str("  ");
        session.out.write_str(command.name);
        // Pad the name column to a fixed width so the summaries line up,
        // clamping an over-long name rather than overflowing.
        let pad = 12usize.saturating_sub(command.name.len());
        for _ in 0..pad.max(1) {
            session.out.write_bytes(b" ");
        }
        session.out.line(command.summary);
    }
    session
        .out
        .line("Type `help <command>` for details on one command.");
    Flow::Stay
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::MockSession;

    #[test]
    fn tokenize_splits_on_spaces_and_tabs() {
        let mut tokens: [&[u8]; MAX_TOKENS] = [&[]; MAX_TOKENS];
        let n = tokenize(b"  mem   map\t\tx ", &mut tokens);
        assert_eq!(n, 3);
        assert_eq!(tokens[0], b"mem");
        assert_eq!(tokens[1], b"map");
        assert_eq!(tokens[2], b"x");
    }

    #[test]
    fn tokenize_of_empty_line_is_zero() {
        let mut tokens: [&[u8]; MAX_TOKENS] = [&[]; MAX_TOKENS];
        assert_eq!(tokenize(b"   \t ", &mut tokens), 0);
    }

    #[test]
    fn tokenize_folds_overflow_into_the_last_token() {
        // More words than MAX_TOKENS: the tail is preserved in the last slot.
        let mut line = alloc::string::String::new();
        for i in 0..(MAX_TOKENS + 5) {
            if i > 0 {
                line.push(' ');
            }
            line.push('w');
        }
        let mut tokens: [&[u8]; MAX_TOKENS] = [&[]; MAX_TOKENS];
        let n = tokenize(line.as_bytes(), &mut tokens);
        assert_eq!(n, MAX_TOKENS);
        // The final token holds the remaining words (space-separated).
        assert!(tokens[MAX_TOKENS - 1].contains(&b' '));
    }

    #[test]
    fn every_command_name_is_lowercase_and_unique() {
        for command in COMMANDS {
            assert!(command.name.bytes().all(|b| !b.is_ascii_uppercase()));
        }
        for (i, a) in COMMANDS.iter().enumerate() {
            for b in &COMMANDS[i + 1..] {
                assert!(!a.matches(b.name.as_bytes()), "duplicate: {}", a.name);
            }
        }
    }

    #[test]
    fn dispatch_matches_case_insensitively() {
        let mut session = MockSession::new(&[]);
        let flow = dispatch(b"HELP", &mut session.session());
        assert_eq!(flow, Flow::Stay);
        assert!(session.output_contains("Supervisor commands:"));
    }

    #[test]
    fn dispatch_of_empty_line_is_a_stay_no_op() {
        let mut session = MockSession::new(&[]);
        assert_eq!(dispatch(b"   ", &mut session.session()), Flow::Stay);
        assert!(session.output().is_empty());
    }

    #[test]
    fn unknown_command_is_reported_not_panicked() {
        let mut session = MockSession::new(&[]);
        assert_eq!(
            dispatch(b"frobnicate now", &mut session.session()),
            Flow::Stay
        );
        assert!(session.output_contains("unknown command"));
    }

    #[test]
    fn help_for_one_command_shows_its_body() {
        let mut session = MockSession::new(&[]);
        dispatch(b"help mount", &mut session.session());
        assert!(session.output_contains("real root unlock"));
    }

    #[test]
    fn help_for_unknown_topic_is_reported() {
        let mut session = MockSession::new(&[]);
        dispatch(b"help nope", &mut session.session());
        assert!(session.output_contains("no such command"));
    }
}
