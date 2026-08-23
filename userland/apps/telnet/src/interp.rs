//! The `telnet>` command interpreter.
//!
//! The escape character suspends the relay and hands the terminal to this
//! interpreter, whose command set and spellings track BSD/inetutils `telnet` —
//! including its unambiguous-prefix abbreviation, so `o host`, `q` and `st`
//! work as they always have.
//!
//! Two BSD commands are deliberately absent, and their absence is documented
//! rather than faked. `!` (shell escape) would mean giving a program that
//! parses hostile network input the authority to spawn a shell, which inverts
//! the minimum-capability posture the tool is built on. `slc check` has no wire
//! form in RFC 1184 distinct from `slc export`, so offering both would be two
//! names for one action.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::command::{parse_escape, parse_port, Target, DEFAULT_PORT};
use crate::linemode::{mode, slc_function, slc_name, SLC_MAX, SLC_NOVALUE};
use crate::nvt;
use crate::option::{self, option_name};
use crate::session::Session;
use crate::subneg::{Environ, EnvironFault};

/// What a command asks the caller to do. Everything the interpreter can do
/// itself — printing, editing the session's state, queueing bytes for the wire
/// — it has already done by the time it returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Prompt again: the command was handled and the operator stays in command
    /// mode.
    Prompt,
    /// Return to relaying the terminal.
    Resume,
    /// Close the connection, staying in command mode.
    Close,
    /// Close the connection and end the program.
    Quit,
    /// Connect to this target, closing any current connection first.
    Open(Target),
    /// Stop this process until it is continued.
    Suspend,
}

/// The top-level command words, in the order `?` lists them.
const COMMANDS: &[(&str, &str)] = &[
    ("close", "close the current connection"),
    ("display", "show the operating parameters"),
    (
        "environ",
        "manage the environment variables NEW-ENVIRON discloses",
    ),
    ("logout", "ask the remote host to log this session out"),
    (
        "mode",
        "try to enter a line-by-line or character-at-a-time mode",
    ),
    ("open", "connect to a host"),
    ("quit", "close the connection and exit"),
    ("send", "transmit a telnet command"),
    ("set", "set an operating parameter"),
    ("slc", "manage the special-character (SLC) table"),
    ("status", "show the connection status"),
    ("toggle", "flip an operating parameter"),
    ("unset", "clear an operating parameter"),
    ("z", "suspend telnet"),
    ("?", "list the commands"),
];

/// The `send` arguments and what each transmits.
const SENDABLE: &[(&str, u8)] = &[
    ("abort", nvt::ABORT),
    ("ao", nvt::AO),
    ("ayt", nvt::AYT),
    ("brk", nvt::BRK),
    ("ec", nvt::EC),
    ("el", nvt::EL),
    ("eof", nvt::XEOF),
    ("eor", nvt::EOR),
    ("ga", nvt::GA),
    ("ip", nvt::IP),
    ("nop", nvt::NOP),
    ("susp", nvt::SUSP),
    ("synch", nvt::DM),
];

/// The `toggle` arguments.
const TOGGLES: &[&str] = &[
    "autoflush",
    "autosynch",
    "binary",
    "crlf",
    "crmod",
    "debug",
    "inbinary",
    "localchars",
    "netdata",
    "options",
    "outbinary",
    "?",
];

/// The `mode` arguments.
const MODES: &[&str] = &[
    "character",
    "edit",
    "-edit",
    "isig",
    "-isig",
    "line",
    "litecho",
    "-litecho",
    "softtab",
    "-softtab",
    "?",
];

/// How a word matched a command table.
enum Matched<'a> {
    /// Exactly one entry matched.
    One(&'a str),
    /// The prefix names more than one entry.
    Ambiguous,
    /// Nothing matched.
    Unknown,
}

/// Resolve `word` against `names`, accepting an exact match or an unambiguous
/// prefix (the abbreviation BSD telnet's own command reader allows).
fn lookup<'a>(word: &str, names: impl Iterator<Item = &'a str>) -> Matched<'a> {
    let mut found: Option<&'a str> = None;
    for name in names {
        if name == word {
            return Matched::One(name);
        }
        if name.starts_with(word) {
            if found.is_some() {
                return Matched::Ambiguous;
            }
            found = Some(name);
        }
    }
    found.map_or(Matched::Unknown, Matched::One)
}

/// Execute one `telnet>` command line against `session`.
///
/// An empty line resumes the relay, exactly as BSD telnet does, so pressing
/// Return at the prompt gets the operator back to the session.
pub fn execute(line: &str, session: &mut Session, connected: bool) -> Action {
    let mut words = line.split_whitespace();
    let Some(word) = words.next() else {
        return Action::Resume;
    };
    let rest: Vec<&str> = words.collect();
    let name = match lookup(word, COMMANDS.iter().map(|&(name, _)| name)) {
        Matched::One(name) => name,
        Matched::Ambiguous => {
            print_line(session, &alloc::format!("?Ambiguous command \"{word}\""));
            return Action::Prompt;
        }
        Matched::Unknown => {
            print_line(session, &alloc::format!("?Invalid command \"{word}\""));
            return Action::Prompt;
        }
    };
    match name {
        "?" => {
            for &(command, help) in COMMANDS {
                print_line(session, &alloc::format!("{command:<9} {help}"));
            }
            Action::Prompt
        }
        "open" => open(session, &rest),
        "close" => {
            if connected {
                Action::Close
            } else {
                print_line(session, "?Need to be connected first.");
                Action::Prompt
            }
        }
        "quit" => Action::Quit,
        "logout" => {
            if connected {
                let mut wire = Vec::new();
                nvt::push_negotiate(nvt::DO, option::LOGOUT, &mut wire);
                session.push_wire(&wire);
                Action::Resume
            } else {
                print_line(session, "?Need to be connected first.");
                Action::Prompt
            }
        }
        "z" => Action::Suspend,
        "status" => {
            status(session, connected);
            Action::Prompt
        }
        "display" => {
            display(session);
            Action::Prompt
        }
        "send" => send(session, &rest, connected),
        "set" => {
            set(session, &rest);
            Action::Prompt
        }
        "unset" => {
            unset(session, &rest);
            Action::Prompt
        }
        "toggle" => {
            toggle(session, &rest);
            Action::Prompt
        }
        "mode" => {
            set_mode(session, &rest, connected);
            Action::Prompt
        }
        "environ" => {
            environ(session, &rest);
            Action::Prompt
        }
        "slc" => {
            slc(session, &rest, connected);
            Action::Prompt
        }
        _ => Action::Prompt,
    }
}

/// `open host [port]`.
fn open(session: &mut Session, args: &[&str]) -> Action {
    let Some(&host) = args.first() else {
        print_line(session, "usage: open host [port]");
        return Action::Prompt;
    };
    let port = match args.get(1) {
        Some(text) => match parse_port(text) {
            Ok(port) => port,
            Err(err) => {
                print_line(session, &alloc::format!("?{err}"));
                return Action::Prompt;
            }
        },
        None => DEFAULT_PORT,
    };
    if args.len() > 2 {
        print_line(session, "usage: open host [port]");
        return Action::Prompt;
    }
    Action::Open(Target {
        host: host.to_string(),
        port,
    })
}

/// `send <arg>...` — every argument is transmitted, in order.
fn send(session: &mut Session, args: &[&str], connected: bool) -> Action {
    if args.is_empty() || args.contains(&"?") {
        for &(name, command) in SENDABLE {
            let label = nvt::command_name(command).unwrap_or("?");
            print_line(
                session,
                &alloc::format!("{name:<9} send the {label} command"),
            );
        }
        print_line(session, "escape    send the escape character");
        print_line(session, "getstatus request the remote option status");
        print_line(session, "do        send DO for an option number");
        print_line(session, "dont      send DONT for an option number");
        print_line(session, "will      send WILL for an option number");
        print_line(session, "wont      send WONT for an option number");
        return Action::Prompt;
    }
    if !connected {
        print_line(session, "?Need to be connected first.");
        return Action::Prompt;
    }
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if let Some(&(_, command)) = SENDABLE.iter().find(|&&(name, _)| name == arg) {
            let mut wire = Vec::new();
            nvt::push_command(command, &mut wire);
            session.push_wire(&wire);
            continue;
        }
        match arg {
            "escape" => {
                if let Some(byte) = session.escape() {
                    let mode = session.transmit_mode();
                    let mut wire = Vec::new();
                    nvt::escape_into(&[byte], mode, &mut wire);
                    session.push_wire(&wire);
                } else {
                    print_line(session, "?There is no escape character.");
                }
            }
            "getstatus" => {
                if session.options().remote(option::STATUS) {
                    let mut wire = Vec::new();
                    nvt::push_subnegotiation(
                        option::STATUS,
                        &[crate::subneg::cmd::SEND],
                        &mut wire,
                    );
                    session.push_wire(&wire);
                } else {
                    print_line(session, "?Remote side does not support STATUS.");
                }
            }
            verb @ ("do" | "dont" | "will" | "wont") => {
                let Some(text) = args.get(index) else {
                    print_line(session, &alloc::format!("usage: send {verb} <option>"));
                    break;
                };
                index += 1;
                match parse_option(text) {
                    Some(opt) => {
                        let byte = match verb {
                            "do" => nvt::DO,
                            "dont" => nvt::DONT,
                            "will" => nvt::WILL,
                            _ => nvt::WONT,
                        };
                        let mut wire = Vec::new();
                        nvt::push_negotiate(byte, opt, &mut wire);
                        session.push_wire(&wire);
                    }
                    None => print_line(
                        session,
                        &alloc::format!("?\"{text}\" is not an option number or name."),
                    ),
                }
            }
            other => print_line(
                session,
                &alloc::format!(
                    "?\"{other}\" is not a valid send argument (\"send ?\" lists them)."
                ),
            ),
        }
    }
    Action::Prompt
}

/// Resolve an option by name or number, so `send do linemode` and `send do 34`
/// both work.
fn parse_option(text: &str) -> Option<u8> {
    if let Ok(number) = text.parse::<u8>() {
        return Some(number);
    }
    (0u8..=255).find(|&code| {
        option_name(code).is_some_and(|name| name.eq_ignore_ascii_case(text))
            || matches!(option_name(code), Some(name) if name.replace(' ', "-").eq_ignore_ascii_case(text))
    })
}

/// `set <var> <value>`.
fn set(session: &mut Session, args: &[&str]) {
    let (Some(&var), value) = (args.first(), args.get(1)) else {
        print_settables(session);
        return;
    };
    if var == "?" {
        print_settables(session);
        return;
    }
    if var == "escape" {
        let Some(&value) = value else {
            print_line(session, "usage: set escape <character>");
            return;
        };
        match parse_escape(value) {
            Some(escape) => {
                session.set_escape(escape);
                print_line(
                    session,
                    &alloc::format!("Escape character is {}.", render_char_opt(escape)),
                );
            }
            None => print_line(
                session,
                &alloc::format!("?\"{value}\" is not a character or \"^X\" spelling."),
            ),
        }
        return;
    }
    if var == "echo" {
        let Some(&value) = value else {
            print_line(session, "usage: set echo <character>");
            return;
        };
        match parse_escape(value) {
            // The local-echo toggle character is BSD telnet's; without a
            // separate escape prefix there is nothing for it to toggle here, so
            // the honest answer is that it is not settable rather than storing
            // a value that never acts.
            Some(_) => print_line(
                session,
                "?This client has no echo-toggle character; use \"toggle ...\" instead.",
            ),
            None => print_line(session, &alloc::format!("?\"{value}\" is not a character.")),
        }
        return;
    }
    let Some(function) = slc_function(var) else {
        print_line(
            session,
            &alloc::format!("?\"{var}\" is not a settable variable."),
        );
        return;
    };
    let Some(&value) = value else {
        print_line(session, &alloc::format!("usage: set {var} <character>"));
        return;
    };
    match parse_escape(value) {
        Some(Some(byte)) if session.linemode_mut().slc_mut().set_local(function, byte) => {
            print_line(
                session,
                &alloc::format!("{var} character is {}.", render_char(byte)),
            );
        }
        Some(Some(_)) => print_line(
            session,
            &alloc::format!("?The remote host pinned the {var} character."),
        ),
        Some(None) => print_line(session, &alloc::format!("usage: set {var} <character>")),
        None => print_line(session, &alloc::format!("?\"{value}\" is not a character.")),
    }
}

/// `unset <var>`.
fn unset(session: &mut Session, args: &[&str]) {
    let Some(&var) = args.first() else {
        print_settables(session);
        return;
    };
    if var == "?" {
        print_settables(session);
        return;
    }
    if var == "escape" {
        session.set_escape(None);
        print_line(session, "There is no escape character.");
        return;
    }
    match slc_function(var) {
        Some(function) if session.linemode_mut().slc_mut().unset_local(function) => {
            print_line(session, &alloc::format!("{var} character is disabled."));
        }
        Some(_) => print_line(
            session,
            &alloc::format!("?The remote host pinned the {var} character."),
        ),
        None => print_line(
            session,
            &alloc::format!("?\"{var}\" is not a settable variable."),
        ),
    }
}

/// List everything `set`/`unset` accepts.
fn print_settables(session: &mut Session) {
    print_line(session, "escape    the character that enters command mode");
    for function in 1..=SLC_MAX {
        if let Some(name) = slc_name(function) {
            print_line(
                session,
                &alloc::format!("{name:<9} the {name} special character"),
            );
        }
    }
}

/// `toggle <arg>...`.
fn toggle(session: &mut Session, args: &[&str]) {
    if args.is_empty() || args.contains(&"?") {
        for &name in TOGGLES {
            if name != "?" {
                print_line(session, name);
            }
        }
        return;
    }
    for &arg in args {
        let name = match lookup(arg, TOGGLES.iter().copied()) {
            Matched::One(name) => name,
            Matched::Ambiguous => {
                print_line(session, &alloc::format!("?Ambiguous argument \"{arg}\""));
                continue;
            }
            Matched::Unknown => {
                print_line(session, &alloc::format!("?\"{arg}\" is not a toggle."));
                continue;
            }
        };
        // The three BINARY toggles are negotiations, not local flags: they ask
        // the remote side and its answer decides.
        if matches!(name, "binary" | "inbinary" | "outbinary") {
            binary_toggle(session, name);
            continue;
        }
        let flags = session.flags_mut();
        let now = match name {
            "autoflush" => {
                flags.autoflush = !flags.autoflush;
                flags.autoflush
            }
            "autosynch" => {
                flags.autosynch = !flags.autosynch;
                flags.autosynch
            }
            "crlf" => {
                flags.crlf = !flags.crlf;
                flags.crlf
            }
            "crmod" => {
                flags.crmod = !flags.crmod;
                flags.crmod
            }
            "localchars" => {
                flags.localchars = !flags.localchars;
                flags.localchars
            }
            "netdata" => {
                flags.netdata = !flags.netdata;
                flags.netdata
            }
            // `debug` and `options` both mean "trace the negotiation" here:
            // there is no socket-level debugging to switch on, so `debug` is
            // wired to the trace it can actually produce.
            _ => {
                flags.options = !flags.options;
                flags.options
            }
        };
        print_line(
            session,
            &alloc::format!("{name} {}.", if now { "enabled" } else { "disabled" }),
        );
    }
}

/// Ask the remote side to turn BINARY on or off in the named direction.
fn binary_toggle(session: &mut Session, name: &str) {
    let mut wire = Vec::new();
    let (out_side, in_side) = match name {
        "outbinary" => (true, false),
        "inbinary" => (false, true),
        _ => (true, true),
    };
    if out_side {
        let enabled = session.options().local(option::BINARY);
        let options = session.options_mut();
        let result = if enabled {
            options.ask_local_disable(option::BINARY, &mut wire)
        } else {
            options.ask_local_enable(option::BINARY, &mut wire)
        };
        if result.is_err() {
            print_line(
                session,
                "?The outbound BINARY negotiation is already under way.",
            );
        }
    }
    if in_side {
        let enabled = session.options().remote(option::BINARY);
        let options = session.options_mut();
        let result = if enabled {
            options.ask_remote_disable(option::BINARY, &mut wire)
        } else {
            options.ask_remote_enable(option::BINARY, &mut wire)
        };
        if result.is_err() {
            print_line(
                session,
                "?The inbound BINARY negotiation is already under way.",
            );
        }
    }
    session.push_wire(&wire);
}

/// `mode <arg>`.
fn set_mode(session: &mut Session, args: &[&str], connected: bool) {
    if args.is_empty() || args.contains(&"?") {
        for &name in MODES {
            if name != "?" {
                print_line(session, name);
            }
        }
        return;
    }
    if !connected {
        print_line(session, "?Need to be connected first.");
        return;
    }
    for &arg in args {
        let name = match lookup(arg, MODES.iter().copied()) {
            Matched::One(name) => name,
            Matched::Ambiguous => {
                print_line(session, &alloc::format!("?Ambiguous argument \"{arg}\""));
                continue;
            }
            Matched::Unknown => {
                print_line(session, &alloc::format!("?\"{arg}\" is not a mode."));
                continue;
            }
        };
        let bit = match name {
            "edit" | "-edit" | "line" | "character" => mode::EDIT,
            "isig" | "-isig" => mode::TRAPSIG,
            "softtab" | "-softtab" => mode::SOFT_TAB,
            _ => mode::LIT_ECHO,
        };
        let wanted = !name.starts_with('-') && name != "character";
        if session.options().local(option::LINEMODE) {
            let mask = if wanted {
                session.linemode().mask() | bit
            } else {
                session.linemode().mask() & !bit
            };
            let mut wire = Vec::new();
            session.linemode_mut().request_mode(mask, &mut wire);
            session.push_wire(&wire);
            continue;
        }
        // Without LINEMODE, "line" and "character" are the historical
        // negotiation: a character-at-a-time server suppresses Go Ahead and
        // echoes; a line-at-a-time one does neither. The other mode bits have
        // no meaning without the option, and saying so beats pretending.
        if !matches!(name, "line" | "character") {
            print_line(
                session,
                &alloc::format!("?\"{name}\" needs the LINEMODE option, which is not in force."),
            );
            continue;
        }
        let character = name == "character";
        let mut wire = Vec::new();
        let options = session.options_mut();
        if character {
            let _ = options.ask_remote_enable(option::SUPPRESS_GO_AHEAD, &mut wire);
            let _ = options.ask_remote_enable(option::ECHO, &mut wire);
        } else {
            let _ = options.ask_remote_disable(option::SUPPRESS_GO_AHEAD, &mut wire);
            let _ = options.ask_remote_disable(option::ECHO, &mut wire);
        }
        session.push_wire(&wire);
    }
}

/// `environ <define|undefine|export|unexport|list>`.
fn environ(session: &mut Session, args: &[&str]) {
    const SUBCOMMANDS: &[&str] = &["define", "export", "list", "undefine", "unexport", "?"];
    let Some(&word) = args.first() else {
        print_line(
            session,
            "usage: environ define|undefine|export|unexport|list",
        );
        return;
    };
    let sub = match lookup(word, SUBCOMMANDS.iter().copied()) {
        Matched::One(name) => name,
        Matched::Ambiguous => {
            print_line(session, &alloc::format!("?Ambiguous argument \"{word}\""));
            return;
        }
        Matched::Unknown => {
            print_line(
                session,
                &alloc::format!("?\"{word}\" is not an environ command."),
            );
            return;
        }
    };
    match sub {
        "?" => print_line(
            session,
            "define <var> <value> | undefine <var> | export <var> | unexport <var> | list",
        ),
        "list" => {
            if session.environ().vars().is_empty() {
                print_line(session, "No environment variables are defined.");
                return;
            }
            let lines: Vec<String> = session
                .environ()
                .vars()
                .iter()
                .map(|var| {
                    alloc::format!(
                        "{} {}={}",
                        if var.exported { "export" } else { "      " },
                        var.name,
                        var.value
                    )
                })
                .collect();
            for line in lines {
                print_line(session, &line);
            }
        }
        "define" => {
            let (Some(&name), Some(&value)) = (args.get(1), args.get(2)) else {
                print_line(session, "usage: environ define <var> <value>");
                return;
            };
            match session.environ_mut().define(name, value) {
                Ok(()) => print_line(
                    session,
                    &alloc::format!("{name} defined (not yet exported)."),
                ),
                Err(fault) => print_line(session, &alloc::format!("?{}", environ_fault(fault))),
            }
        }
        "undefine" => apply_environ(session, args.get(1), Environ::undefine, "undefined"),
        "export" => apply_environ(
            session,
            args.get(1),
            |env: &mut Environ, name: &str| env.set_exported(name, true),
            "exported",
        ),
        _ => apply_environ(
            session,
            args.get(1),
            |env: &mut Environ, name: &str| env.set_exported(name, false),
            "no longer exported",
        ),
    }
}

/// Apply one name-taking `environ` sub-command, reporting the outcome once.
fn apply_environ(
    session: &mut Session,
    name: Option<&&str>,
    action: impl FnOnce(&mut Environ, &str) -> Result<(), EnvironFault>,
    done: &str,
) {
    let Some(&name) = name else {
        print_line(session, "usage: environ <command> <var>");
        return;
    };
    match action(session.environ_mut(), name) {
        Ok(()) => print_line(session, &alloc::format!("{name} {done}.")),
        Err(fault) => print_line(session, &alloc::format!("?{}", environ_fault(fault))),
    }
}

/// The operator-facing text for an environment refusal.
fn environ_fault(fault: EnvironFault) -> &'static str {
    match fault {
        EnvironFault::TableFull => "No room for another environment variable.",
        EnvironFault::NameLength => "The variable name is empty or too long.",
        EnvironFault::ValueLength => "The value is too long.",
        EnvironFault::NotPrintable => "A name or value may hold only printable characters.",
        EnvironFault::Unknown => "No such variable is defined.",
    }
}

/// `slc <export|import>`.
fn slc(session: &mut Session, args: &[&str], connected: bool) {
    const SUBCOMMANDS: &[&str] = &["export", "import", "?"];
    let Some(&word) = args.first() else {
        print_line(session, "usage: slc export|import");
        return;
    };
    let sub = match lookup(word, SUBCOMMANDS.iter().copied()) {
        Matched::One(name) => name,
        Matched::Ambiguous => {
            print_line(session, &alloc::format!("?Ambiguous argument \"{word}\""));
            return;
        }
        Matched::Unknown => {
            print_line(
                session,
                &alloc::format!("?\"{word}\" is not an slc command."),
            );
            return;
        }
    };
    if sub == "?" {
        print_line(session, "export    state this client's special characters");
        print_line(session, "import    ask the remote host for its own");
        return;
    }
    if !connected || !session.options().local(option::LINEMODE) {
        print_line(session, "?The LINEMODE option is not in force.");
        return;
    }
    let mut wire = Vec::new();
    if sub == "export" {
        session.linemode().slc().push_export(&mut wire);
    } else {
        // Asking the remote host for its own values is RFC 1184's `DEFAULT`
        // level: it means "use yours", and the reply states them.
        let mut params = alloc::vec![crate::linemode::sub::SLC];
        for function in 1..=SLC_MAX {
            params.extend_from_slice(&[function, crate::linemode::slc_flag::DEFAULT, SLC_NOVALUE]);
        }
        nvt::push_subnegotiation(option::LINEMODE, &params, &mut wire);
    }
    session.push_wire(&wire);
}

/// `status`.
fn status(session: &mut Session, connected: bool) {
    if !connected {
        print_line(session, "No connection.");
    }
    if session.options().local(option::LINEMODE) {
        print_line(session, "Operating with the LINEMODE option");
        let (edit, trapsig, soft_tab, lit_echo) = {
            let lm = session.linemode();
            (lm.edit(), lm.trapsig(), lm.soft_tab(), lm.lit_echo())
        };
        print_line(
            session,
            if edit {
                "Local line editing"
            } else {
                "Remote character echo"
            },
        );
        if trapsig {
            print_line(session, "Local signal handling");
        }
        if soft_tab {
            print_line(session, "Local tab expansion");
        }
        if lit_echo {
            print_line(session, "Literal echo of control characters");
        }
    } else if session.options().remote(option::ECHO) {
        print_line(session, "Operating in character-at-a-time mode");
    } else {
        print_line(session, "Operating in line-by-line mode");
    }
    if session.options().local(option::BINARY) {
        print_line(session, "Operating in binary mode on transmit");
    }
    if session.options().remote(option::BINARY) {
        print_line(session, "Operating in binary mode on receive");
    }
    let escape = render_char_opt(session.escape());
    print_line(session, &alloc::format!("Escape character is {escape}."));
}

/// `display`.
fn display(session: &mut Session) {
    let flags = *session.flags();
    let rows: &[(&str, bool)] = &[
        (
            "flush output when sending interrupt characters",
            flags.autoflush,
        ),
        (
            "send a Synch when sending interrupt characters",
            flags.autosynch,
        ),
        ("map carriage return on output", flags.crmod),
        ("send the line terminator as CR LF", flags.crlf),
        ("recognize the local special characters", flags.localchars),
        ("trace option negotiation", flags.options),
        ("trace non-data network traffic", flags.netdata),
        (
            "honour the negotiated flow-control characters",
            flags.flow_control,
        ),
    ];
    for &(text, on) in rows {
        print_line(
            session,
            &alloc::format!("{} {text}.", if on { "will" } else { "won't" }),
        );
    }
    print_line(session, "");
    let escape = render_char_opt(session.escape());
    print_line(session, &alloc::format!("escape    {escape}"));
    let table = session.linemode().slc().clone();
    for function in 1..=SLC_MAX {
        let (Some(name), Some(entry)) = (slc_name(function), table.get(function)) else {
            continue;
        };
        let value = if entry.active() {
            render_char(entry.value)
        } else {
            String::from("(disabled)")
        };
        print_line(session, &alloc::format!("{name:<9} {value}"));
    }
}

/// Render one character in the `^X` / `'c'` spelling every operator-facing
/// surface uses — `display`, `status`, and the connect banner.
#[must_use]
pub fn render_char(byte: u8) -> String {
    match byte {
        0x7F => String::from("'^?'"),
        0x00..=0x1F => alloc::format!("'^{}'", char::from(byte | 0x40)),
        printable if printable.is_ascii_graphic() || printable == b' ' => {
            alloc::format!("'{}'", char::from(printable))
        }
        other => alloc::format!("'\\{other:03o}'"),
    }
}

/// Render an optional character, naming the absence honestly.
#[must_use]
pub fn render_char_opt(byte: Option<u8>) -> String {
    byte.map_or_else(|| String::from("(none)"), render_char)
}

/// Write one line to the session's terminal output, with the `CR LF` a raw-mode
/// terminal needs.
fn print_line(session: &mut Session, text: &str) {
    session.print(text);
    session.print("\r\n");
}

#[cfg(test)]
mod tests;
