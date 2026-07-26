//! TAIRiX `configure` — read and set the boot-time system-configuration
//! store (`plans/APPS.md`).
//!
//! The `sysctl`-shaped settings command: with no operand it lists every
//! setting of the closed registry with its current value; with a key it
//! shows that setting; with a key and a value it updates the store at
//! `/System/Settings/Configuration/system.conf`
//! (`tairix_sysconfig::CONFIG_PATH`). The store's grammar, key registry,
//! fail-closed parse, and canonical render are the shared `lib/sysconfig`
//! engine — the same engine every boot-time consumer reads through, so this
//! writer and those readers can never diverge.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool: the [`parse`]r that maps a
//! command line to a [`Command`], and the [`run`] engine that executes it
//! against injected seams:
//!
//! * [`Store`] — read and replace the store document (the `Run` binary
//!   wires the syscall-backed file at `CONFIG_PATH`; tests wire an
//!   in-memory fixture).
//! * [`Output`] — write listings and values to the terminal.
//! * `HelpSource` (from `lib/help`) — the tool's own bundle `Help/` tree,
//!   rendered by the `-h`/`-?`/`--help` switches through the one shared
//!   engine (never embedded help text).
//!
//! # Fail closed
//!
//! An unknown option or an extra operand is a usage error that changes
//! nothing; an unknown key or a value outside its key's closed set is
//! refused with the valid choices stated; a store document the shared
//! engine cannot fully parse refuses a *set* outright (never a guessed
//! merge) while a *list*/*show* reports the malformation honestly. A
//! refused store read or write surfaces the underlying
//! [`Errno`] — a permission denial changes nothing and
//! states its reason. No panic, no partial application.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! crate, the shared `lib/help` engine, and the shared `lib/sysconfig`
//! engine, so this userland tool never links a kernel or driver crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths; nothing
//! writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};
use tairix_sysconfig::{Key, SystemConfig};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `configure`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: configure [<key> [<value>]] [-h | -?]";

/// One thing the `configure` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command<'a> {
    /// List every registry setting and its current value.
    List,
    /// Show one setting's current value.
    Show(&'a str),
    /// Set one setting to a value.
    Set(&'a str, &'a str),
    /// Render `configure`'s own short help (`-h`/`-?`/`--help`) through
    /// the same engine as any other command's short help (plans/APPS.md).
    Help,
}

/// Why a command line or an operation was refused.
///
/// Every variant is a fail-closed refusal: nothing was changed, and the
/// rendered message states the reason (and, for a value refusal, the valid
/// choices).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigureError {
    /// The command line was not understood.
    Usage,
    /// The named key is outside the closed registry.
    UnknownKey,
    /// The value is outside the named key's closed set.
    InvalidValue(Key),
    /// The store document on disk could not be fully parsed by the shared
    /// engine (a hand edit outside the grammar); a set refuses rather than
    /// guess at a merge.
    Malformed(tairix_sysconfig::ConfigError),
    /// The store could not be read.
    Read(Errno),
    /// The store could not be written (e.g. the caller may not change
    /// system settings).
    Write(Errno),
    /// The terminal output could not be delivered.
    Output(Errno),
}

impl fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("command line not understood"),
            Self::UnknownKey => f.write_str("unknown setting (run `configure` to list them)"),
            Self::InvalidValue(key) => {
                write!(f, "invalid value for {}; valid:", key.name())?;
                for value in key.values() {
                    write!(f, " {value}")?;
                }
                Ok(())
            }
            Self::Malformed(err) => write!(f, "store not understood: {err}"),
            Self::Read(err) => write!(f, "cannot read the store: {err}"),
            Self::Write(err) => write!(f, "cannot write the store: {err}"),
            Self::Output(err) => write!(f, "cannot write output: {err}"),
        }
    }
}

/// Reads and replaces the configuration-store document.
///
/// The `Run` binary wires the syscall-backed file at
/// `tairix_sysconfig::CONFIG_PATH`; tests wire an in-memory fixture. The
/// document travels whole in both directions: the engine's canonical render
/// replaces the file, never a partial patch.
pub trait Store {
    /// Read the whole store document, or `None` when no store exists yet
    /// (a fresh installation — the defaults apply).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises other than absence.
    fn read(&self) -> Result<Option<String>, Errno>;

    /// Replace the store document with `text` (creating it, and its
    /// directory, when absent).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — notably
    /// [`Errno::PermissionDenied`] when the caller may not change system
    /// settings.
    fn write(&self, text: &str) -> Result<(), Errno>;
}

/// Writes bytes to the tool's standard output.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed consumer).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `configure [<key> [<value>]] [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md; they win immediately).
/// * no operand — list every setting.
/// * one operand — show that setting.
/// * two operands — set that setting.
/// * anything else — a [`ConfigureError::Usage`] error: the tool defines
///   no other options.
///
/// # Errors
///
/// [`ConfigureError::Usage`] for any input outside the grammar above.
pub fn parse<'a>(args: &[&'a str]) -> Result<Command<'a>, ConfigureError> {
    let mut operands: [Option<&'a str>; 2] = [None, None];
    let mut count = 0usize;
    for arg in args {
        match *arg {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            other if other.starts_with('-') => return Err(ConfigureError::Usage),
            other => {
                if count >= operands.len() {
                    return Err(ConfigureError::Usage);
                }
                operands[count] = Some(other);
                count += 1;
            }
        }
    }
    Ok(match (operands[0], operands[1]) {
        (None, _) => Command::List,
        (Some(key), None) => Command::Show(key),
        (Some(key), Some(value)) => Command::Set(key, value),
    })
}

/// Execute `command` against the injected seams.
///
/// # Errors
///
/// The [`ConfigureError`] naming the refusal; nothing was changed and
/// nothing partial was written.
pub fn run(
    command: Command<'_>,
    locale: Option<&str>,
    store: &dyn Store,
    help: &dyn HelpSource,
    output: &dyn Output,
) -> Result<(), ConfigureError> {
    match command {
        Command::Help => {
            // The tool's own Help document through the one shared engine;
            // the usage banner stands in when no document can be served (a
            // build without the bundle's documents) so `-h` never fails.
            let bytes = own_short_help(help, locale, "configure")
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            output.write_all(&bytes).map_err(ConfigureError::Output)
        }
        Command::List => {
            let config = load(store)?;
            let mut text = String::new();
            for key in Key::ALL {
                text.push_str(key.name());
                text.push(' ');
                text.push_str(config.get(*key));
                text.push('\n');
            }
            output
                .write_all(text.as_bytes())
                .map_err(ConfigureError::Output)
        }
        Command::Show(name) => {
            let key = Key::from_name(name).ok_or(ConfigureError::UnknownKey)?;
            let config = load(store)?;
            let text = format!("{}\n", config.get(key));
            output
                .write_all(text.as_bytes())
                .map_err(ConfigureError::Output)
        }
        Command::Set(name, value) => {
            let key = Key::from_name(name).ok_or(ConfigureError::UnknownKey)?;
            let mut config = load(store)?;
            config
                .set(key, value)
                .map_err(|_| ConfigureError::InvalidValue(key))?;
            store.write(&config.render()).map_err(ConfigureError::Write)
        }
    }
}

/// Read and parse the current store, or the defaults when none exists.
///
/// A document the shared engine cannot fully parse is a
/// [`ConfigureError::Malformed`] refusal: the tool never guesses at a
/// partial intent, and a later set never merges into a document it did not
/// understand.
fn load(store: &dyn Store) -> Result<SystemConfig, ConfigureError> {
    match store.read().map_err(ConfigureError::Read)? {
        Some(text) => SystemConfig::parse(&text).map_err(ConfigureError::Malformed),
        None => Ok(SystemConfig::default()),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::Errno;
    use tairix_help::HelpSource;
    use tairix_sysconfig::{Key, SystemConfig};

    use super::{parse, run, Command, ConfigureError, Output, Store, USAGE};

    /// An in-memory store fixture: `None` models the fresh installation.
    struct MemStore {
        text: RefCell<Option<String>>,
        read_err: Option<Errno>,
        write_err: Option<Errno>,
    }

    impl MemStore {
        fn new(text: Option<&str>) -> Self {
            Self {
                text: RefCell::new(text.map(String::from)),
                read_err: None,
                write_err: None,
            }
        }
    }

    impl Store for MemStore {
        fn read(&self) -> Result<Option<String>, Errno> {
            match self.read_err {
                Some(err) => Err(err),
                None => Ok(self.text.borrow().clone()),
            }
        }

        fn write(&self, text: &str) -> Result<(), Errno> {
            if let Some(err) = self.write_err {
                return Err(err);
            }
            *self.text.borrow_mut() = Some(text.to_string());
            Ok(())
        }
    }

    /// A capturing output fixture.
    #[derive(Default)]
    struct MemOutput {
        bytes: RefCell<Vec<u8>>,
    }

    impl Output for MemOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    impl MemOutput {
        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf-8 output")
        }
    }

    /// A help source with no documents, so the usage banner stands in.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, tairix_help::SourceError> {
            Ok(Vec::new())
        }
        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, tairix_help::SourceError> {
            Ok(None)
        }
    }

    #[test]
    fn parse_maps_the_grammar() {
        assert_eq!(parse(&[]), Ok(Command::List));
        assert_eq!(parse(&["os.loginType"]), Ok(Command::Show("os.loginType")));
        assert_eq!(
            parse(&["os.loginType", "graphical"]),
            Ok(Command::Set("os.loginType", "graphical")),
        );
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // Help wins wherever it appears.
        assert_eq!(parse(&["os.loginType", "-h"]), Ok(Command::Help));
    }

    #[test]
    fn parse_refuses_extra_operands_and_unknown_options() {
        assert_eq!(parse(&["a", "b", "c"]), Err(ConfigureError::Usage));
        assert_eq!(parse(&["--frob"]), Err(ConfigureError::Usage));
        assert_eq!(parse(&["-x"]), Err(ConfigureError::Usage));
    }

    #[test]
    fn list_shows_defaults_for_a_fresh_installation() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        run(Command::List, None, &store, &NoHelp, &output).expect("lists");
        assert_eq!(
            output.text(),
            "os.loginType text\n\
             cache.all on\n\
             cache.filesystem auto\n\
             cache.block auto\n\
             cache.transform auto\n\
             cache.semantic auto\n\
             net.ipv4.enabled true\n\
             net.ipv6.enabled true\n\
             net.ipv6.privacy false\n\
             net.tcp.syncookies auto\n\
             net.tcp.keepalive false\n",
        );
    }

    #[test]
    fn show_reports_the_stored_value() {
        let store = MemStore::new(Some("os.loginType graphical\n"));
        let output = MemOutput::default();
        run(
            Command::Show("os.loginType"),
            None,
            &store,
            &NoHelp,
            &output,
        )
        .expect("shows");
        assert_eq!(output.text(), "graphical\n");
    }

    #[test]
    fn set_writes_the_canonical_render_and_round_trips() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        run(
            Command::Set("os.loginType", "graphical"),
            None,
            &store,
            &NoHelp,
            &output,
        )
        .expect("sets");
        let text = store.text.borrow().clone().expect("store written");
        let config = SystemConfig::parse(&text).expect("canonical render parses");
        assert_eq!(config.get(Key::LoginType), "graphical");
        // Nothing goes to stdout on a successful set (the GNU quiet
        // convention).
        assert_eq!(output.text(), "");
    }

    #[test]
    fn unknown_key_fails_closed_without_touching_the_store() {
        let store = MemStore::new(Some("os.loginType text\n"));
        let output = MemOutput::default();
        assert_eq!(
            run(
                Command::Set("os.frob", "on"),
                None,
                &store,
                &NoHelp,
                &output
            ),
            Err(ConfigureError::UnknownKey),
        );
        assert_eq!(
            store.text.borrow().as_deref(),
            Some("os.loginType text\n"),
            "a refused set changes nothing"
        );
    }

    #[test]
    fn invalid_value_names_the_valid_choices() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        let err = run(
            Command::Set("os.loginType", "desktop"),
            None,
            &store,
            &NoHelp,
            &output,
        )
        .expect_err("refused");
        assert_eq!(err, ConfigureError::InvalidValue(Key::LoginType));
        assert_eq!(
            format!("{err}"),
            "invalid value for os.loginType; valid: text graphical",
        );
        assert!(store.text.borrow().is_none(), "nothing was written");
    }

    #[test]
    fn a_malformed_store_refuses_a_set_rather_than_merging() {
        let store = MemStore::new(Some("os.unknownKey what\n"));
        let output = MemOutput::default();
        let err = run(
            Command::Set("os.loginType", "text"),
            None,
            &store,
            &NoHelp,
            &output,
        )
        .expect_err("refused");
        assert!(matches!(err, ConfigureError::Malformed(_)));
        assert_eq!(
            store.text.borrow().as_deref(),
            Some("os.unknownKey what\n"),
            "the malformed document is left untouched"
        );
    }

    #[test]
    fn store_errors_surface_with_their_errno() {
        let mut store = MemStore::new(None);
        store.read_err = Some(Errno::PermissionDenied);
        let output = MemOutput::default();
        assert_eq!(
            run(Command::List, None, &store, &NoHelp, &output),
            Err(ConfigureError::Read(Errno::PermissionDenied)),
        );

        let mut store = MemStore::new(None);
        store.write_err = Some(Errno::PermissionDenied);
        assert_eq!(
            run(
                Command::Set("os.loginType", "graphical"),
                None,
                &store,
                &NoHelp,
                &output,
            ),
            Err(ConfigureError::Write(Errno::PermissionDenied)),
        );
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_documents() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        run(Command::Help, None, &store, &NoHelp, &output).expect("help renders");
        assert_eq!(output.text(), format!("{USAGE}\n"));
    }

    /// Every locale's Help document names the settings this registry
    /// defines and the reserved short-help switches (`plans/APPS.md`): the
    /// key tokens are language-neutral, so each translated document must
    /// carry the same keys as the canonical one. The documents are read
    /// from the bundle's own on-disk `Help/` tree — the single source the
    /// image builder plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_registry_keys_and_switches() {
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        for locale in tairix_help::REQUIRED_LOCALES {
            let path = format!("{help_root}/{locale}/configure.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for token in ["`os.loginType`", "`-h, -?`"] {
                assert!(
                    text.contains(token),
                    "{locale}/configure.md must document {token}"
                );
            }
            for key in Key::ALL {
                assert!(
                    text.contains(key.name()),
                    "{locale}/configure.md must document {}",
                    key.name()
                );
            }
        }
    }
}
