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
//! # Two registries, one command line
//!
//! A key name resolves against the flat `lib/sysconfig` registry first and,
//! failing that, against the per-interface `<iface>.<suffix>` registry of
//! the network store at `/System/Settings/Network/network.conf`
//! (`tairix_netconfig::CONFIG_PATH`). The flat registry always wins, so no
//! interface alias can ever shadow a machine setting — and the two name
//! sets are pinned disjoint by a test over both registries rather than left
//! to the accident that none collides today.
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
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::net_ipc::NetworkSettings;
use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};
use tairix_netconfig::{IfaceKey, NetworkConfig};
use tairix_sysconfig::{Key, SystemConfig, ValueShape};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `configure`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: configure [<key> [<value> [<key> <value>]...]] [-h | -?]";

/// Most `<key> <value>` pairs one invocation may set.
///
/// The registry is closed and small, so setting every key at once is the
/// widest an invocation can meaningfully be; a longer command line names a
/// key twice and is refused rather than applied in some order.
pub const MAX_PAIRS: usize = tairix_sysconfig::Key::ALL.len();

/// One thing the `configure` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command<'a> {
    /// List every registry setting and its current value.
    List,
    /// Show one setting's current value.
    Show(&'a str),
    /// Set each named setting to the value beside it, together.
    ///
    /// Several pairs rather than one, because the store is rendered and
    /// replaced whole: applying a group of changes one invocation at a time
    /// could leave it holding half of them if a later one were refused.
    Set(Vec<(&'a str, &'a str)>),
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
    /// The network store could not be read.
    NetworkRead(Errno),
    /// The network store document could not be fully parsed by the shared
    /// engine; a listing reports the malformation rather than showing a
    /// document it did not understand.
    NetworkMalformed(tairix_netconfig::ParseError),
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
            Self::InvalidValue(key) => match key.shape() {
                ValueShape::Closed(values) => {
                    write!(f, "invalid value for {}; valid:", key.name())?;
                    for value in values {
                        write!(f, " {value}")?;
                    }
                    Ok(())
                }
                ValueShape::Free(form) => {
                    write!(f, "invalid value for {}; expected {form}", key.name())
                }
            },
            Self::Malformed(err) => write!(f, "store not understood: {err}"),
            Self::Read(err) => write!(f, "cannot read the store: {err}"),
            Self::NetworkRead(err) => write!(f, "cannot read the network store: {err}"),
            Self::NetworkMalformed(err) => {
                write!(f, "network store not understood: {err}")
            }
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

/// Reads the per-interface network-configuration document.
///
/// Separate from [`Store`] because it is a different document with a
/// different engine (`lib/netconfig`), not a second view of the same one.
/// The `Run` binary wires the syscall-backed file at
/// `tairix_netconfig::CONFIG_PATH`; tests wire an in-memory fixture.
pub trait NetworkStore {
    /// Read the whole network document, or `None` when none exists yet (no
    /// managed interfaces — the engine's own default).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises other than absence — notably
    /// [`Errno::PermissionDenied`], since the document carries each
    /// interface's hardware identity and this machine's static addressing
    /// and is not world-readable.
    fn read(&self) -> Result<Option<String>, Errno>;
}

/// Applies the stack-wide `net.*` policy to the running network stack.
///
/// Writing the store persists a change for the next boot; the running stack
/// only learns of it when the policy is delivered over its capability-gated
/// admin endpoint. The `Run` binary backs this with that call; tests wire a
/// recorder. It is a seam rather than a direct call so the tool stays
/// host-testable, and it grants nothing: the kernel gates the endpoint on the
/// caller's `CAP_NET_ADMIN`.
pub trait NetPolicy {
    /// Deliver `settings` to the running network stack.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the endpoint raises — notably
    /// [`Errno::PermissionDenied`] when the caller does not hold
    /// `CAP_NET_ADMIN`, or [`Errno::NotFound`] when no network stack is
    /// running. The store write has already succeeded either way.
    fn apply(&self, settings: NetworkSettings) -> Result<(), Errno>;
}

/// Writes bytes to one of the tool's output streams.
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
    let mut operands: Vec<&'a str> = Vec::new();
    for arg in args {
        match *arg {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            other if other.starts_with('-') => return Err(ConfigureError::Usage),
            other => operands.push(other),
        }
    }
    match operands.len() {
        0 => Ok(Command::List),
        1 => Ok(Command::Show(operands[0])),
        // An odd count past the first leaves a key with no value, which is
        // a command line that says nothing rather than one to guess at.
        len if len % 2 == 1 => Err(ConfigureError::Usage),
        len if len / 2 > MAX_PAIRS => Err(ConfigureError::Usage),
        _ => Ok(Command::Set(
            operands
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect(),
        )),
    }
}

/// Execute `command` against the injected seams.
///
/// # Errors
///
/// The [`ConfigureError`] naming the refusal; nothing was changed and
/// nothing partial was written.
#[allow(clippy::too_many_arguments)] // Each seam is injected separately so every branch stays host-testable.
pub fn run(
    command: Command<'_>,
    locale: Option<&str>,
    store: &dyn Store,
    network: &dyn NetworkStore,
    policy: &dyn NetPolicy,
    help: &dyn HelpSource,
    output: &dyn Output,
    diagnostics: &dyn Output,
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
                text.push_str(&config.render_value(*key));
                text.push('\n');
            }
            // Then every per-interface key the network store actually
            // holds. Only the set ones: that document has no defaults to
            // print, and a wall of unset keys would say nothing.
            for iface in load_network(network)?.interfaces() {
                for key in IfaceKey::ALL {
                    if let Some(value) = iface.render_value(*key) {
                        text.push_str(&iface.name);
                        text.push('.');
                        text.push_str(key.name());
                        text.push(' ');
                        text.push_str(&value);
                        text.push('\n');
                    }
                }
            }
            output
                .write_all(text.as_bytes())
                .map_err(ConfigureError::Output)
        }
        Command::Show(name) => {
            let text = match resolve(name)? {
                Named::System(key) => format!("{}\n", load(store)?.render_value(key)),
                // An unset per-interface key has no value to show: the
                // document holds only what was written, so the answer is
                // the empty one rather than a default this registry does
                // not have.
                Named::Interface(iface, key) => format!(
                    "{}\n",
                    load_network(network)?
                        .interface(iface)
                        .and_then(|found| found.render_value(key))
                        .unwrap_or_default()
                ),
            };
            output
                .write_all(text.as_bytes())
                .map_err(ConfigureError::Output)
        }
        Command::Set(pairs) => {
            // Every pair is resolved and applied to one working copy before
            // a byte is written, so a command line that names an unknown
            // key or an invalid value changes nothing at all rather than
            // applying the pairs that came before it.
            let mut config = load(store)?;
            let mut network = false;
            let mut seen: Vec<Key> = Vec::with_capacity(pairs.len());
            for (name, value) in &pairs {
                let key = Key::from_name(name).ok_or(ConfigureError::UnknownKey)?;
                // A key named twice is a command line with two intents for
                // one setting; there is no order in which both are honoured.
                if seen.contains(&key) {
                    return Err(ConfigureError::Usage);
                }
                seen.push(key);
                config
                    .set(key, value)
                    .map_err(|_| ConfigureError::InvalidValue(key))?;
                network |= key.is_network();
            }
            store
                .write(&config.render())
                .map_err(ConfigureError::Write)?;
            if !network {
                return Ok(());
            }
            // A `net.*` key describes the running stack, so persisting it is
            // only half the change. Applying it is a separate, refusable
            // action: a refusal (no stack running, or no `CAP_NET_ADMIN`)
            // leaves the saved setting standing for the next boot and is
            // reported rather than fatal.
            match policy.apply(config.network_settings()) {
                Ok(()) => Ok(()),
                // A diagnostic, so it never lands in the stdout a script
                // parses. One notice for the whole policy, because one
                // delivery carries all of it.
                Err(err) => diagnostics
                    .write_all(deferred_notice(&seen, err).as_bytes())
                    .map_err(ConfigureError::Output),
            }
        }
    }
}

/// The notice a saved-but-not-applied `net.*` change reports: the settings
/// are persisted, the running stack did not take them, and why.
fn deferred_notice(keys: &[Key], err: Errno) -> String {
    let mut named = String::new();
    for key in keys.iter().filter(|key| key.is_network()) {
        if !named.is_empty() {
            named.push_str(", ");
        }
        named.push_str(key.name());
    }
    format!(
        "{named}: saved; the running network stack did not accept it ({err}); it applies at next \
         boot\n"
    )
}

/// Which registry a key name on the command line belongs to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Named<'a> {
    /// A key of the flat machine-configuration registry.
    System(Key),
    /// An interface alias and a key of the per-interface registry.
    Interface(&'a str, IfaceKey),
}

/// Resolve a key name against the two registries, flat one first.
///
/// The order is what makes a machine setting unshadowable: an interface
/// alias is operator-chosen text, so a store that named one `net` must
/// still never take `net.ipv4.enabled` away from the machine registry.
///
/// # Errors
///
/// [`ConfigureError::UnknownKey`] when the name is in neither registry.
fn resolve(name: &str) -> Result<Named<'_>, ConfigureError> {
    if let Some(key) = Key::from_name(name) {
        return Ok(Named::System(key));
    }
    let (iface, suffix) = name.split_once('.').ok_or(ConfigureError::UnknownKey)?;
    // The alias grammar is the engine's, not a second copy of it here.
    if !tairix_netconfig::valid_iface_name(iface) {
        return Err(ConfigureError::UnknownKey);
    }
    let key = IfaceKey::from_name(suffix).ok_or(ConfigureError::UnknownKey)?;
    Ok(Named::Interface(iface, key))
}

/// Read and parse the current network store, or the empty configuration
/// when none exists.
///
/// A document the shared engine cannot fully parse is a
/// [`ConfigureError::NetworkMalformed`] refusal, for the same reason the
/// machine store's is: this tool never guesses at a partial intent.
fn load_network(store: &dyn NetworkStore) -> Result<NetworkConfig, ConfigureError> {
    match store.read().map_err(ConfigureError::NetworkRead)? {
        Some(text) => NetworkConfig::parse(&text).map_err(ConfigureError::NetworkMalformed),
        None => Ok(NetworkConfig::default()),
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

    use tairix_abi::net_ipc::NetworkSettings;
    use tairix_abi::Errno;
    use tairix_help::HelpSource;
    use tairix_sysconfig::{Key, SystemConfig};

    use super::{
        parse, run, Command, ConfigureError, NetPolicy, NetworkStore, Output, Store, USAGE,
    };

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

    /// An in-memory network store fixture: `None` models "no managed
    /// interfaces", which is what an absent document means.
    struct MemNetStore {
        text: Option<String>,
        read_err: Option<Errno>,
    }

    impl MemNetStore {
        fn empty() -> Self {
            Self {
                text: None,
                read_err: None,
            }
        }

        fn holding(text: &str) -> Self {
            Self {
                text: Some(String::from(text)),
                read_err: None,
            }
        }

        fn refusing(err: Errno) -> Self {
            Self {
                text: None,
                read_err: Some(err),
            }
        }
    }

    impl NetworkStore for MemNetStore {
        fn read(&self) -> Result<Option<String>, Errno> {
            match self.read_err {
                Some(err) => Err(err),
                None => Ok(self.text.clone()),
            }
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

    /// A recording [`NetPolicy`]: captures each delivered policy and
    /// answers with a scripted result.
    struct MemPolicy {
        applied: RefCell<Vec<NetworkSettings>>,
        result: Result<(), Errno>,
    }

    impl MemPolicy {
        fn accepting() -> Self {
            Self {
                applied: RefCell::new(Vec::new()),
                result: Ok(()),
            }
        }

        fn refusing(err: Errno) -> Self {
            Self {
                applied: RefCell::new(Vec::new()),
                result: Err(err),
            }
        }
    }

    impl NetPolicy for MemPolicy {
        fn apply(&self, settings: NetworkSettings) -> Result<(), Errno> {
            self.applied.borrow_mut().push(settings);
            self.result
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
            Ok(Command::Set(alloc::vec![("os.loginType", "graphical")])),
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
        let errors = MemOutput::default();
        run(
            Command::List,
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &errors,
        )
        .expect("lists");
        assert_eq!(
            output.text(),
            "os.loginType graphical\n\
             cache.all on\n\
             cache.filesystem auto\n\
             cache.block auto\n\
             cache.transform auto\n\
             cache.semantic auto\n\
             net.ipv4.enabled true\n\
             net.ipv6.enabled true\n\
             net.ipv6.privacy false\n\
             net.tcp.syncookies auto\n\
             net.tcp.keepalive false\n\
             net.tcp.ecn false\n\
             time.servers none\n\
             time.refresh 1d\n\
             input.mouse.debounce 25\n",
        );
    }

    #[test]
    fn show_reports_the_stored_value() {
        let store = MemStore::new(Some("os.loginType graphical\n"));
        let output = MemOutput::default();
        let errors = MemOutput::default();
        run(
            Command::Show("os.loginType"),
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &errors,
        )
        .expect("shows");
        assert_eq!(output.text(), "graphical\n");
    }

    #[test]
    fn set_writes_the_canonical_render_and_round_trips() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        let errors = MemOutput::default();
        run(
            Command::Set(alloc::vec![("os.loginType", "graphical")]),
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &errors,
        )
        .expect("sets");
        let text = store.text.borrow().clone().expect("store written");
        let config = SystemConfig::parse(&text).expect("canonical render parses");
        assert_eq!(config.render_value(Key::LoginType), "graphical");
        // Nothing goes to stdout on a successful set (the GNU quiet
        // convention).
        assert_eq!(output.text(), "");
    }

    #[test]
    fn setting_a_net_key_applies_it_to_the_running_stack() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        let errors = MemOutput::default();
        let policy = MemPolicy::accepting();
        run(
            Command::Set(alloc::vec![("net.tcp.ecn", "true")]),
            None,
            &store,
            &MemNetStore::empty(),
            &policy,
            &NoHelp,
            &output,
            &errors,
        )
        .expect("sets");
        // Persisting alone would only take effect at the next boot, and the
        // stack holds no filesystem capability to read the store itself.
        let applied = policy.applied.borrow();
        assert_eq!(applied.len(), 1);
        assert!(applied[0].tcp_ecn);
        // The whole policy travels, not just the changed key, so the stack's
        // view can never drift from the document.
        assert!(applied[0].ipv4_enabled && applied[0].ipv6_enabled);
        assert_eq!(output.text(), "", "a delivered change says nothing");
        assert_eq!(errors.text(), "", "and reports no diagnostic");
    }

    #[test]
    fn setting_a_non_net_key_leaves_the_stack_alone() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        let errors = MemOutput::default();
        let policy = MemPolicy::accepting();
        run(
            Command::Set(alloc::vec![("os.loginType", "text")]),
            None,
            &store,
            &MemNetStore::empty(),
            &policy,
            &NoHelp,
            &output,
            &errors,
        )
        .expect("sets");
        assert!(
            policy.applied.borrow().is_empty(),
            "an os.* key is no business of the network stack"
        );
    }

    #[test]
    fn a_refused_live_apply_keeps_the_saved_setting_and_says_so() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        let errors = MemOutput::default();
        // No network stack running (or no CAP_NET_ADMIN): the refusal is an
        // answer about one action, not a failure of the command.
        let policy = MemPolicy::refusing(Errno::NotFound);
        run(
            Command::Set(alloc::vec![("net.ipv6.privacy", "true")]),
            None,
            &store,
            &MemNetStore::empty(),
            &policy,
            &NoHelp,
            &output,
            &errors,
        )
        .expect("the setting is still saved");
        assert_eq!(
            store.text.borrow().as_deref(),
            Some(
                &*SystemConfig::parse("net.ipv6.privacy true\n")
                    .expect("parses")
                    .render()
            )
        );
        // Loud, not silent: the operator is told the running stack did not
        // take it and when it will — on the diagnostic stream, so a script
        // parsing stdout is unaffected.
        assert_eq!(output.text(), "", "stdout carries no diagnostic");
        let text = errors.text();
        assert!(text.contains("net.ipv6.privacy"), "{text}");
        assert!(text.contains("next boot"), "{text}");
    }

    #[test]
    fn unknown_key_fails_closed_without_touching_the_store() {
        let store = MemStore::new(Some("os.loginType text\n"));
        let output = MemOutput::default();
        let errors = MemOutput::default();
        assert_eq!(
            run(
                Command::Set(alloc::vec![("os.frob", "on")]),
                None,
                &store,
                &MemNetStore::empty(),
                &MemPolicy::accepting(),
                &NoHelp,
                &output,
                &errors
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
        let errors = MemOutput::default();
        let err = run(
            Command::Set(alloc::vec![("os.loginType", "desktop")]),
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &errors,
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
        let errors = MemOutput::default();
        let err = run(
            Command::Set(alloc::vec![("os.loginType", "text")]),
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &errors,
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
        let errors = MemOutput::default();
        assert_eq!(
            run(
                Command::List,
                None,
                &store,
                &MemNetStore::empty(),
                &MemPolicy::accepting(),
                &NoHelp,
                &output,
                &errors
            ),
            Err(ConfigureError::Read(Errno::PermissionDenied)),
        );

        let mut store = MemStore::new(None);
        store.write_err = Some(Errno::PermissionDenied);
        assert_eq!(
            run(
                Command::Set(alloc::vec![("os.loginType", "graphical")]),
                None,
                &store,
                &MemNetStore::empty(),
                &MemPolicy::accepting(),
                &NoHelp,
                &output,
                &errors,
            ),
            Err(ConfigureError::Write(Errno::PermissionDenied)),
        );
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_documents() {
        let store = MemStore::new(None);
        let output = MemOutput::default();
        let errors = MemOutput::default();
        run(
            Command::Help,
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &errors,
        )
        .expect("help renders");
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

    #[test]
    fn several_pairs_are_applied_to_one_rendered_document() {
        // What makes a settings surface's Apply atomic: the whole change is
        // one invocation, so the store is never left holding half of it.
        let store = MemStore::new(None);
        let out = MemOutput::default();
        run(
            Command::Set(alloc::vec![
                ("os.loginType", "text"),
                ("cache.all", "off"),
                ("cache.block", "off"),
            ]),
            None,
            &store,
            &MemNetStore::empty(),
            &MemPolicy::accepting(),
            &NoHelp,
            &out,
            &MemOutput::default(),
        )
        .expect("sets");
        let text = store.text.borrow().clone().expect("a store");
        let config = SystemConfig::parse(&text).expect("parses");
        assert_eq!(config.login_type, tairix_sysconfig::LoginType::Text);
        assert_eq!(config.cache_all, tairix_sysconfig::CacheSwitch::Off);
        assert_eq!(config.cache_block, tairix_sysconfig::CacheMode::Off);
    }

    #[test]
    fn a_refused_pair_changes_nothing_at_all() {
        // Resolved and applied to a working copy before a byte is written,
        // so a later bad pair cannot leave the earlier ones standing.
        let store = MemStore::new(Some("os.loginType graphical\n"));
        for command in [
            Command::Set(alloc::vec![("cache.all", "off"), ("no.such.key", "x")]),
            Command::Set(alloc::vec![("cache.all", "off"), ("cache.block", "wat")]),
        ] {
            let refused = run(
                command,
                None,
                &store,
                &MemNetStore::empty(),
                &MemPolicy::accepting(),
                &NoHelp,
                &MemOutput::default(),
                &MemOutput::default(),
            );
            assert!(refused.is_err());
            assert_eq!(
                store.text.borrow().as_deref(),
                Some("os.loginType graphical\n")
            );
        }
    }

    /// One managed interface, spelled the way the store holds it.
    const ONE_INTERFACE: &str = "wan.kind ethernet\n\
         wan.match.mac 02:00:00:00:00:01\n\
         wan.ipv4.method static\n\
         wan.ipv4.address 10.0.0.7/24\n\
         wan.ipv4.gateway 10.0.0.1\n";

    #[test]
    fn list_states_both_registries_flat_first() {
        let output = MemOutput::default();
        run(
            Command::List,
            None,
            &MemStore::new(None),
            &MemNetStore::holding(ONE_INTERFACE),
            &MemPolicy::accepting(),
            &NoHelp,
            &output,
            &MemOutput::default(),
        )
        .expect("lists");
        let text = output.text();
        let (flat, interfaces) = text
            .split_once("wan.kind")
            .expect("the per-interface lines follow the flat registry");
        // Every flat key, defaults included, and no interface line among
        // them.
        for key in Key::ALL {
            assert!(flat.contains(key.name()), "{} missing: {flat}", key.name());
        }
        assert!(!flat.contains("wan."), "an interface line came first");
        // Then only the keys the document actually holds — that registry
        // has no defaults to print.
        assert!(
            interfaces.contains("wan.ipv4.address 10.0.0.7/24"),
            "{interfaces}"
        );
        assert!(!interfaces.contains("wan.mtu"), "an unset key was printed");
    }

    #[test]
    fn show_reads_a_per_interface_key_and_answers_nothing_for_an_unset_one() {
        for (key, want) in [
            ("wan.ipv4.address", "10.0.0.7/24\n"),
            ("wan.kind", "ethernet\n"),
            // Unset, and on a declared interface: the document holds only
            // what was written, so there is no value and none is invented.
            ("wan.mtu", "\n"),
            // An interface the document never declared, likewise.
            ("lan0.ipv4.address", "\n"),
        ] {
            let output = MemOutput::default();
            run(
                Command::Show(key),
                None,
                &MemStore::new(None),
                &MemNetStore::holding(ONE_INTERFACE),
                &MemPolicy::accepting(),
                &NoHelp,
                &output,
                &MemOutput::default(),
            )
            .expect("shows");
            assert_eq!(output.text(), want, "{key}");
        }
    }

    #[test]
    fn a_name_in_neither_registry_is_refused() {
        for key in ["wan.nonsense", "nonsense", "wan..kind", ".kind"] {
            assert_eq!(
                run(
                    Command::Show(key),
                    None,
                    &MemStore::new(None),
                    &MemNetStore::holding(ONE_INTERFACE),
                    &MemPolicy::accepting(),
                    &NoHelp,
                    &MemOutput::default(),
                    &MemOutput::default(),
                ),
                Err(ConfigureError::UnknownKey),
                "{key}"
            );
        }
    }

    #[test]
    fn the_two_registries_name_nothing_in_common() {
        // An interface alias is operator-chosen text, so `net` is a legal
        // one and `net.<suffix>` could in principle collide with a machine
        // key. Pinned here rather than left to the accident that none does
        // today: a new key on either side that collided would be resolved
        // by the flat registry and the interface key made unreachable.
        for key in Key::ALL {
            let Some((iface, suffix)) = key.name().split_once('.') else {
                continue;
            };
            assert!(
                tairix_netconfig::IfaceKey::from_name(suffix).is_none()
                    || !tairix_netconfig::valid_iface_name(iface),
                "{} reads as an interface key too",
                key.name()
            );
        }
    }

    #[test]
    fn a_refused_or_malformed_network_store_fails_closed_and_says_which() {
        let refused = run(
            Command::List,
            None,
            &MemStore::new(None),
            &MemNetStore::refusing(Errno::PermissionDenied),
            &MemPolicy::accepting(),
            &NoHelp,
            &MemOutput::default(),
            &MemOutput::default(),
        );
        assert_eq!(
            refused,
            Err(ConfigureError::NetworkRead(Errno::PermissionDenied))
        );
        assert_eq!(
            format!("{}", refused.expect_err("refused")),
            "cannot read the network store: permission denied"
        );

        let malformed = run(
            Command::Show("wan.mtu"),
            None,
            &MemStore::new(None),
            &MemNetStore::holding("wan.nonsense 1\n"),
            &MemPolicy::accepting(),
            &NoHelp,
            &MemOutput::default(),
            &MemOutput::default(),
        );
        assert!(matches!(
            malformed,
            Err(ConfigureError::NetworkMalformed(_))
        ));
    }

    #[test]
    fn the_command_line_grammar_admits_pairs_and_refuses_a_lone_key() {
        assert_eq!(
            parse(&["os.loginType", "text", "cache.all", "off"]),
            Ok(Command::Set(alloc::vec![
                ("os.loginType", "text"),
                ("cache.all", "off")
            ]))
        );
        // An odd operand past the first leaves a key with no value.
        assert_eq!(
            parse(&["os.loginType", "text", "cache.all"]),
            Err(ConfigureError::Usage)
        );
        // And a key named twice has two intents for one setting.
        assert_eq!(
            parse(&["cache.all", "on", "cache.all", "off"]).map(|command| run(
                command,
                None,
                &MemStore::new(None),
                &MemNetStore::empty(),
                &MemPolicy::accepting(),
                &NoHelp,
                &MemOutput::default(),
                &MemOutput::default(),
            )),
            Ok(Err(ConfigureError::Usage))
        );
    }
}
