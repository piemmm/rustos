//! TAIRiX `applib` — administer the desktop's program-library catalog
//! (`plans/NEW-TASKBAR.md` T2/T3).
//!
//! The library-admin command: `applib` lists the resolved library exactly as
//! the desktop's launcher shows it (machine store ∪ the caller's overlay);
//! `applib add`/`remove` register and unregister an application bundle;
//! `applib hide`/`show` set an entry's visibility verdict; `applib rescan`
//! walks the application stores and registers every listed bundle the
//! catalog does not know yet — discovery, never a compiled-in list. The
//! machine-wide store lives at `tairix_proglib::LIBRARY_PATH` and the
//! per-user overlay in this application's own *published* app-data scope
//! (`store::AppDataStore`, `plans/APPDATA.md` §1.1) — two backings of one
//! [`Store`] seam, so this tool's editing logic never learns where a catalog
//! lives. Every document is read and written through the one `lib/proglib`
//! registry, so this writer and the desktop's readers can never diverge.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool: the [`parse`]r that maps a
//! command line to a [`Command`], and the [`run`] engine that executes it
//! against injected seams:
//!
//! * [`Store`] — read and replace one catalog document (the `Run` binary
//!   wires the syscall-backed machine store and, when the environment names
//!   a home, the caller's own overlay; tests wire in-memory fixtures).
//! * [`Bundles`] — list a directory and read a bundle's `AppInfo` manifest,
//!   bounded by [`tairix_abi::APPINFO_WIRE_MAX`] (the `Run` binary wires the
//!   secured VFS; tests wire an in-memory tree).
//! * [`Output`] — write listings to standard output and emit the fd-3
//!   `stdinfo` advisory records (best-effort, never load-bearing).
//! * `HelpSource` (from `lib/help`) — the tool's own bundle `Help/` tree,
//!   rendered by the `-h`/`-?` switches through the one shared engine.
//!
//! # Fail closed
//!
//! An unknown option, folder, or entry is a refusal that changes nothing; a
//! store document the shared engine cannot fully parse refuses the whole
//! operation rather than guessing at a merge; a refused read or write
//! surfaces the underlying [`Errno`] and states which store it concerns. A
//! `rescan` skips a bundle whose manifest is unreadable, over-long, or
//! undecodable — one bad bundle never aborts the scan — and fails closed on
//! a store tree larger than the walk bounds rather than reporting a partial
//! truth. Registering, hiding, or showing an entry changes *presentation
//! only*: launching a bundle stays behind the loader's signature and
//! capability gate regardless of what the catalog says.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! crate, the shared `lib/help` engine, and the shared `lib/proglib`
//! engine, so this userland tool never links a kernel or driver crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod store;
pub use store::AppDataStore;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_abi::{
    AppInfoHeader, Errno, LibraryCategory, BUNDLE_SUFFIX, HOME_APPLICATION_STORE_DIR,
    HOME_COMMAND_STORE_DIR, INSTALLED_APP_STORE, SYSTEM_APPLICATION_STORE, SYSTEM_COMMAND_STORE,
};
use tairix_appconf::Document;
use tairix_help::{own_short_help, HelpSource};
use tairix_proglib::{
    document as catalog_document, load as load_catalog, merge, BundlePath, Catalog, CatalogError,
    DisplayName, EntryError, EntryId, EntryPatch, IconAsset, LibraryEntry, Record,
};

/// The command word: the record producer on fd 3 and the diagnostic prefix.
pub const OWN_WORD: &str = "applib";

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `applib`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: applib [list [--category <folder>]]\n       applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]\n       applib remove <id|bundle> [--user]\n       applib hide <id> [--user]\n       applib show <id> [--user]\n       applib rescan [--user]";

/// Depth bound on the `rescan` store walk. Bundles may be filed in nested
/// plain subdirectories, but a pathological tree must not recurse without
/// limit: directories deeper than this are not descended into.
pub const MAX_WALK_DEPTH: usize = 8;

/// Bound on directory entries a single `rescan` examines across all roots.
/// Ample for real stores — the catalog itself holds at most
/// `tairix_proglib::MAX_ENTRIES` records — while a hostile tree exhausts
/// the bound and fails the scan closed rather than walking forever.
pub const MAX_WALK_ENTRIES: usize = 4 * tairix_proglib::MAX_ENTRIES;

/// One thing the `applib` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command<'a> {
    /// List the resolved library — machine store ∪ the caller's overlay,
    /// exactly what the desktop's launcher shows — optionally one folder.
    List {
        /// Show only this folder.
        category: Option<LibraryCategory>,
    },
    /// Register (or update) one bundle in a catalog.
    Add(AddRequest<'a>),
    /// Drop one record from a catalog.
    Remove {
        /// The entry identifier, or the bundle path it was registered with.
        target: &'a str,
        /// Edit the caller's own overlay instead of the machine store.
        user: bool,
    },
    /// Hide one entry from the resolved library.
    Hide {
        /// The entry identifier.
        id: &'a str,
        /// Record the verdict in the caller's own overlay.
        user: bool,
    },
    /// Re-show one entry a store hid.
    Show {
        /// The entry identifier.
        id: &'a str,
        /// Record the verdict in the caller's own overlay.
        user: bool,
    },
    /// Walk the application stores and register every listed bundle the
    /// catalog does not know yet.
    Rescan {
        /// Rescan the caller's own `<home>/Commands` and `<home>/Applications`
        /// into their overlay instead of the machine stores into the machine
        /// catalog.
        user: bool,
    },
    /// Render `applib`'s own short help (`-h`/`-?`/`--help`) through the
    /// same engine as any other command's short help (plans/APPS.md).
    Help,
}

/// Everything an `applib add` names: the bundle and its optional overrides.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddRequest<'a> {
    /// The `.app` bundle path to register.
    pub bundle: &'a str,
    /// The folder to file it under; defaults to the folder the bundle's
    /// own manifest declares.
    pub category: Option<LibraryCategory>,
    /// The display name; defaults to the manifest's bundle name.
    pub name: Option<&'a str>,
    /// The icon asset; defaults to the manifest's library icon.
    pub icon: Option<&'a str>,
    /// Write the caller's own overlay instead of the machine store.
    pub user: bool,
}

/// Which catalog document an operation touched — named in every store
/// diagnostic so a refusal says *which* file it concerns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// The machine-wide store every account reads.
    Machine,
    /// The caller's own per-user overlay.
    User,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Machine => "machine store",
            Self::User => "user overlay",
        })
    }
}

/// Why a command line or an operation was refused.
///
/// Every variant is a fail-closed refusal: nothing was changed, and the
/// rendered message states the reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppLibError {
    /// The command line was not understood.
    Usage,
    /// A `--category` value outside the closed folder taxonomy.
    UnknownFolder,
    /// The named entry (or bundle) is in no catalog record.
    UnknownEntry,
    /// `add` without `--category` on a bundle whose manifest declares no
    /// library folder: the tool never guesses a folder.
    NotListed,
    /// A `rescan --user` with no usable home directory to walk. Editing the
    /// overlay itself never needs one: the app-data service resolves the
    /// account from the identity the kernel attests, so a caller whose
    /// account has no home is refused by the *service*, in its own words.
    NoHome,
    /// The `add` operand has no readable `AppInfo` manifest, so it is not
    /// an application bundle.
    NoManifest,
    /// The `add` operand's `AppInfo` manifest does not decode.
    BadManifest,
    /// An operand was refused by the catalog's own entry model (a malformed
    /// identifier, name, icon, or bundle path).
    Entry(EntryError),
    /// The catalog already holds its maximum number of records.
    Full,
    /// The `rescan` walk exhausted [`MAX_WALK_ENTRIES`]; the store tree is
    /// not believable and nothing was changed.
    TreeTooLarge,
    /// A store document could not be fully parsed by the shared engine (a
    /// hand edit outside the grammar); the operation refuses rather than
    /// guess.
    Malformed(Side, CatalogError),
    /// A store could not be read.
    Read(Side, Errno),
    /// A store could not be written (e.g. the caller may not change the
    /// machine-wide catalog).
    Write(Side, Errno),
    /// A bundle or store-tree read failed while resolving an explicit
    /// operand or walking a store root.
    Bundle(Errno),
    /// The terminal output could not be delivered.
    Output(Errno),
}

impl fmt::Display for AppLibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("command line not understood"),
            Self::UnknownFolder => {
                f.write_str("unknown folder; valid:")?;
                for folder in LibraryCategory::ALL {
                    write!(f, " {folder}")?;
                }
                Ok(())
            }
            Self::UnknownEntry => f.write_str("no such library entry (run `applib` to list)"),
            Self::NotListed => {
                f.write_str("the bundle's manifest declares no library folder; give --category")
            }
            Self::NoHome => {
                f.write_str("no home directory in the environment; cannot rescan --user")
            }
            Self::NoManifest => f.write_str("not an application bundle (no AppInfo manifest)"),
            Self::BadManifest => f.write_str("the bundle's AppInfo manifest does not decode"),
            Self::Entry(err) => write!(f, "{err}"),
            Self::Full => f.write_str("the catalog is full"),
            Self::TreeTooLarge => {
                f.write_str("the application-store tree exceeds the rescan bound")
            }
            Self::Malformed(side, err) => write!(f, "the {side} is not understood: {err}"),
            Self::Read(side, err) => write!(f, "cannot read the {side}: {err}"),
            Self::Write(side, err) => write!(f, "cannot write the {side}: {err}"),
            Self::Bundle(err) => write!(f, "cannot read the bundle: {err}"),
            Self::Output(err) => write!(f, "cannot write output: {err}"),
        }
    }
}

/// Reads and replaces one catalog store.
///
/// The two layers are two backings of this one seam, which is what keeps the
/// tool's editing logic ignorant of where a catalog lives: the `Run` binary
/// wires the syscall-backed machine store at `tairix_proglib::LIBRARY_PATH`
/// and, for `--user`, the caller's overlay in this application's *published*
/// app-data scope ([`AppDataStore`]); tests wire in-memory fixtures. The
/// document travels whole in both directions: the engine's canonical render
/// replaces the store, never a partial patch.
pub trait Store {
    /// Read the whole store document, or `None` when the store holds nothing
    /// yet (an empty library — the ordinary fresh state).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the backing raises other than absence.
    fn read(&self) -> Result<Option<Document>, Errno>;

    /// Replace the store's contents with `document`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the backing raises — notably
    /// [`Errno::PermissionDenied`] when the caller may not change the
    /// machine-wide catalog.
    fn write(&self, document: &Document) -> Result<(), Errno>;
}

/// One directory entry the [`Bundles`] seam reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntryInfo {
    /// The entry's name within its directory.
    pub name: String,
    /// Whether the entry is itself a directory.
    pub directory: bool,
}

/// Reads the application stores: directory listings for the `rescan` walk
/// and bundle `AppInfo` manifests for `add`/`rescan`.
///
/// The `Run` binary wires the secured VFS (every path resolution and
/// per-inode permission decision is the kernel's, under the caller's
/// attested identity); tests wire an in-memory tree.
pub trait Bundles {
    /// List one directory, or `None` when the path does not exist (an
    /// absent store root is the ordinary state on a machine without one).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises other than absence.
    fn list_dir(&self, path: &str) -> Result<Option<Vec<DirEntryInfo>>, Errno>;

    /// Read the `AppInfo` manifest inside the bundle directory at `bundle`,
    /// or `None` when no manifest file exists there. The read is bounded:
    /// a file larger than [`tairix_abi::APPINFO_WIRE_MAX`] is refused with
    /// [`Errno::LengthOutOfRange`], never half-read.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises other than absence.
    fn read_appinfo(&self, bundle: &str) -> Result<Option<Vec<u8>>, Errno>;
}

/// Writes the tool's terminal output and advisory records.
pub trait Output {
    /// Write every byte of `bytes` to standard output.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed consumer).
    fn out(&self, bytes: &[u8]) -> Result<(), Errno>;

    /// Emit one framed `stdinfo` record on fd 3. Best-effort by the stream
    /// contract: a missing or refusing consumer is silently ignored, and a
    /// record never changes the command's result.
    fn info(&self, bytes: &[u8]);
}

/// The catalog documents an invocation can reach.
pub struct Stores<'a> {
    /// The machine-wide store.
    pub machine: &'a dyn Store,
    /// The caller's own overlay: this application's published app-data
    /// scope, which the service resolves from the identity the kernel
    /// attests, so it needs no home and is always present.
    pub user: &'a dyn Store,
    /// The caller's home directory (the inherited `HOME`), if any: the
    /// `rescan --user` walk roots `<home>/Commands` and `<home>/Applications`
    /// derive from it.
    pub home: Option<&'a str>,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The reserved short-help switches (`-h`/`-?`/`--help`) win wherever they
/// appear. Long options accept both `--option value` and `--option=value`,
/// and `--` ends option parsing (plans/APPS.md; the GNU convention §16.7).
///
/// # Errors
///
/// [`AppLibError::Usage`] for any input outside the grammar, and
/// [`AppLibError::UnknownFolder`] for a `--category` value outside the
/// closed taxonomy.
pub fn parse<'a>(args: &[&'a str]) -> Result<Command<'a>, AppLibError> {
    if args
        .iter()
        .any(|arg| matches!(*arg, "-h" | "-?" | "--help"))
    {
        return Ok(Command::Help);
    }
    let Some((&word, rest)) = args.split_first() else {
        return Ok(Command::List { category: None });
    };
    match word {
        "list" => {
            let mut options = Options::new(rest);
            let mut category = None;
            while let Some(token) = options.next()? {
                match token {
                    Token::Option("category", value) => category = Some(parse_folder(value)?),
                    _ => return Err(AppLibError::Usage),
                }
            }
            Ok(Command::List { category })
        }
        "add" => {
            let mut options = Options::new(rest);
            let (mut bundle, mut category, mut name, mut icon, mut user) =
                (None, None, None, None, false);
            while let Some(token) = options.next()? {
                match token {
                    Token::Option("category", value) => category = Some(parse_folder(value)?),
                    Token::Option("name", value) => name = Some(value),
                    Token::Option("icon", value) => icon = Some(value),
                    Token::Flag("user") => user = true,
                    Token::Operand(value) if bundle.is_none() => bundle = Some(value),
                    _ => return Err(AppLibError::Usage),
                }
            }
            Ok(Command::Add(AddRequest {
                bundle: bundle.ok_or(AppLibError::Usage)?,
                category,
                name,
                icon,
                user,
            }))
        }
        "remove" | "hide" | "show" => {
            let mut options = Options::new(rest);
            let (mut operand, mut user) = (None, false);
            while let Some(token) = options.next()? {
                match token {
                    Token::Flag("user") => user = true,
                    Token::Operand(value) if operand.is_none() => operand = Some(value),
                    _ => return Err(AppLibError::Usage),
                }
            }
            let operand = operand.ok_or(AppLibError::Usage)?;
            Ok(match word {
                "remove" => Command::Remove {
                    target: operand,
                    user,
                },
                "hide" => Command::Hide { id: operand, user },
                _ => Command::Show { id: operand, user },
            })
        }
        "rescan" => {
            let mut options = Options::new(rest);
            let mut user = false;
            while let Some(token) = options.next()? {
                match token {
                    Token::Flag("user") => user = true,
                    _ => return Err(AppLibError::Usage),
                }
            }
            Ok(Command::Rescan { user })
        }
        _ => Err(AppLibError::Usage),
    }
}

/// One parsed command-line token.
enum Token<'a> {
    /// A `--option` with a value (`--option value` or `--option=value`).
    Option(&'a str, &'a str),
    /// A bare `--option` that takes no value.
    Flag(&'a str),
    /// A positional operand.
    Operand(&'a str),
}

/// The options `applib` defines that take a value; everything else long is
/// a bare flag or unknown.
const VALUE_OPTIONS: [&str; 3] = ["category", "name", "icon"];

/// The bare flags `applib` defines.
const FLAG_OPTIONS: [&str; 1] = ["user"];

/// A tiny GNU-style option cursor over a borrowed argument slice.
struct Options<'s, 'a> {
    items: &'s [&'a str],
    index: usize,
    literal: bool,
}

impl<'s, 'a> Options<'s, 'a> {
    fn new(items: &'s [&'a str]) -> Self {
        Self {
            items,
            index: 0,
            literal: false,
        }
    }

    /// The next token, `None` at the end.
    ///
    /// # Errors
    ///
    /// [`AppLibError::Usage`] on an unknown option, a value option without
    /// a value, or a flag given a `=value`.
    fn next(&mut self) -> Result<Option<Token<'a>>, AppLibError> {
        let Some(&item) = self.items.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        if self.literal || !item.starts_with('-') || item == "-" {
            return Ok(Some(Token::Operand(item)));
        }
        if item == "--" {
            self.literal = true;
            return self.next();
        }
        let Some(long) = item.strip_prefix("--") else {
            return Err(AppLibError::Usage);
        };
        let (name, inline) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        if VALUE_OPTIONS.contains(&name) {
            let value = if let Some(value) = inline {
                value
            } else {
                let Some(&value) = self.items.get(self.index) else {
                    return Err(AppLibError::Usage);
                };
                self.index += 1;
                value
            };
            return Ok(Some(Token::Option(name, value)));
        }
        if FLAG_OPTIONS.contains(&name) && inline.is_none() {
            return Ok(Some(Token::Flag(name)));
        }
        Err(AppLibError::Usage)
    }
}

/// Decode a `--category` value against the closed taxonomy.
fn parse_folder(value: &str) -> Result<LibraryCategory, AppLibError> {
    LibraryCategory::from_id(value).ok_or(AppLibError::UnknownFolder)
}

/// Execute `command` against the injected seams.
///
/// # Errors
///
/// The [`AppLibError`] naming the refusal; nothing was changed and nothing
/// partial was written.
pub fn run(
    command: Command<'_>,
    locale: Option<&str>,
    stores: &Stores<'_>,
    bundles: &dyn Bundles,
    help: &dyn HelpSource,
    output: &dyn Output,
) -> Result<(), AppLibError> {
    match command {
        Command::Help => {
            // The tool's own Help document through the one shared engine;
            // the usage banner stands in when no document can be served (a
            // build without the bundle's documents) so `-h` never fails.
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            output.out(&bytes).map_err(AppLibError::Output)
        }
        Command::List { category } => list(stores, output, category),
        Command::Add(request) => add(stores, bundles, output, &request),
        Command::Remove { target, user } => remove(stores, output, target, user),
        Command::Hide { id, user } => set_visibility(stores, output, id, user, true),
        Command::Show { id, user } => set_visibility(stores, output, id, user, false),
        Command::Rescan { user } => rescan(stores, bundles, output, user),
    }
}

/// Read and parse one side's document, or the empty catalog when none
/// exists. A document the shared engine cannot fully parse is a
/// [`AppLibError::Malformed`] refusal: the tool never guesses at a partial
/// intent, and a later write never merges into a document it did not
/// understand.
fn load(store: &dyn Store, side: Side) -> Result<Catalog, AppLibError> {
    match store.read().map_err(|err| AppLibError::Read(side, err))? {
        Some(document) => load_catalog(&document).map_err(|err| AppLibError::Malformed(side, err)),
        None => Ok(Catalog::default()),
    }
}

/// The store an editing operation targets: the caller's overlay under
/// `--user`, else the machine-wide store.
fn target<'a>(stores: &'a Stores<'a>, user: bool) -> (&'a dyn Store, Side) {
    if user {
        (stores.user, Side::User)
    } else {
        (stores.machine, Side::Machine)
    }
}

/// Render and write `catalog` back to its store.
fn save(store: &dyn Store, side: Side, catalog: &Catalog) -> Result<(), AppLibError> {
    store
        .write(&catalog_document(catalog))
        .map_err(|err| AppLibError::Write(side, err))
}

/// `applib [list]`: the resolved library, folder by folder, exactly what
/// the desktop's launcher shows.
fn list(
    stores: &Stores<'_>,
    output: &dyn Output,
    category: Option<LibraryCategory>,
) -> Result<(), AppLibError> {
    let machine = load(stores.machine, Side::Machine)?;
    let overlay = load(stores.user, Side::User)?;
    let resolved = merge(&machine, &overlay);

    let mut text = String::new();
    for folder in resolved.folders() {
        if category.is_some_and(|wanted| wanted != folder) {
            continue;
        }
        text.push_str(folder.as_str());
        text.push('\n');
        for entry in resolved.folder(folder) {
            text.push_str("  ");
            text.push_str(entry.id().as_str());
            text.push_str("  ");
            text.push_str(entry.name().as_str());
            text.push_str("  ");
            text.push_str(entry.bundle().as_str());
            text.push('\n');
        }
    }
    output.out(text.as_bytes()).map_err(AppLibError::Output)
}

/// `applib add`: register (or update) one bundle in the target catalog,
/// deriving identity, name, folder, and icon from its own signed manifest
/// unless overridden.
fn add(
    stores: &Stores<'_>,
    bundles: &dyn Bundles,
    output: &dyn Output,
    request: &AddRequest<'_>,
) -> Result<(), AppLibError> {
    let (store, side) = target(stores, request.user);
    let mut catalog = load(store, side)?;

    let bundle = request.bundle.strip_suffix('/').unwrap_or(request.bundle);
    let bytes = bundles
        .read_appinfo(bundle)
        .map_err(AppLibError::Bundle)?
        .ok_or(AppLibError::NoManifest)?;
    let header = AppInfoHeader::from_bytes(&bytes).map_err(|_| AppLibError::BadManifest)?;

    let id = EntryId::new(header.bundle_id()).map_err(AppLibError::Entry)?;
    let folder = request
        .category
        .or_else(|| header.library_category())
        .ok_or(AppLibError::NotListed)?;
    let display = DisplayName::new(request.name.unwrap_or_else(|| header.bundle_name()))
        .map_err(AppLibError::Entry)?;
    let icon = match request.icon.or_else(|| header.library_icon()) {
        Some(asset) => Some(IconAsset::new(asset).map_err(AppLibError::Entry)?),
        None => None,
    };
    let path = BundlePath::new(bundle).map_err(AppLibError::Entry)?;

    let entry = LibraryEntry::new(id.clone(), display, path, folder, icon);
    let message = format!("Registered {} under {folder}.", entry.name());
    let ai = change_ai("add", &id, Some(&entry));
    catalog.insert(entry).map_err(|_| AppLibError::Full)?;
    save(store, side, &catalog)?;
    emit(output, "apps.library_entry_added", &message, &ai);
    Ok(())
}

/// `applib remove`: drop one record — named by entry identifier or by the
/// bundle path it was registered with — from the target catalog.
fn remove(
    stores: &Stores<'_>,
    output: &dyn Output,
    target_word: &str,
    user: bool,
) -> Result<(), AppLibError> {
    let (store, side) = target(stores, user);
    let mut catalog = load(store, side)?;

    let id = if target_word.starts_with('/') {
        let want = target_word.strip_suffix('/').unwrap_or(target_word);
        catalog
            .entries()
            .find(|entry| entry.bundle().as_str() == want)
            .map(|entry| entry.id().clone())
            .ok_or(AppLibError::UnknownEntry)?
    } else {
        EntryId::new(target_word).map_err(AppLibError::Entry)?
    };
    let removed = catalog.remove(&id).ok_or(AppLibError::UnknownEntry)?;
    save(store, side, &catalog)?;

    let entry = match &removed {
        Record::Entry(entry) => Some(entry),
        Record::Patch(_) => None,
    };
    let message = match entry {
        Some(entry) => format!("Removed {} from the library.", entry.name()),
        None => format!("Removed the adjustments for {id}."),
    };
    let ai = change_ai("remove", &id, entry);
    emit(output, "apps.library_entry_removed", &message, &ai);
    Ok(())
}

/// `applib hide`/`show`: record a visibility verdict for `id` in the target
/// catalog — on the entry itself when that catalog declares it, else as an
/// overlay patch on the entry another document declares.
fn set_visibility(
    stores: &Stores<'_>,
    output: &dyn Output,
    id: &str,
    user: bool,
    hidden: bool,
) -> Result<(), AppLibError> {
    let (store, side) = target(stores, user);
    let id = EntryId::new(id).map_err(AppLibError::Entry)?;

    // The verdict must name a real record somewhere: a typo'd identifier is
    // refused, not silently written as a patch nothing will ever match.
    let machine = load(stores.machine, Side::Machine)?;
    let overlay = load(stores.user, Side::User)?;
    if machine.get(&id).is_none() && overlay.get(&id).is_none() {
        return Err(AppLibError::UnknownEntry);
    }

    let mut catalog = match side {
        Side::Machine => machine,
        Side::User => overlay,
    };
    if let Some(Record::Entry(entry)) = catalog.get(&id) {
        let mut entry = entry.clone();
        entry.set_hidden(hidden);
        catalog.insert(entry).map_err(|_| AppLibError::Full)?;
    } else {
        let mut patch = catalog
            .entry_patch(&id)
            .map_or_else(EntryPatch::new, Clone::clone);
        patch.set_hidden(hidden);
        catalog
            .patch(id.clone(), patch)
            .map_err(|_| AppLibError::Full)?;
    }
    save(store, side, &catalog)?;

    let (code, message) = if hidden {
        (
            "apps.library_entry_hidden",
            format!("Hid {id} from the library."),
        )
    } else {
        (
            "apps.library_entry_shown",
            format!("Re-showed {id} in the library."),
        )
    };
    let ai = change_ai(if hidden { "hide" } else { "show" }, &id, None);
    emit(output, code, &message, &ai);
    Ok(())
}

/// `applib rescan`: walk the application stores, register every listed
/// bundle the catalog does not know yet, and report what changed.
fn rescan(
    stores: &Stores<'_>,
    bundles: &dyn Bundles,
    output: &dyn Output,
    user: bool,
) -> Result<(), AppLibError> {
    let (store, side) = target(stores, user);
    let roots: Vec<String> = if user {
        let home = stores
            .home
            .map(|home| home.strip_suffix('/').unwrap_or(home))
            .filter(|home| !home.is_empty())
            .ok_or(AppLibError::NoHome)?;
        alloc::vec![
            format!("{home}/{HOME_COMMAND_STORE_DIR}"),
            format!("{home}/{HOME_APPLICATION_STORE_DIR}"),
        ]
    } else {
        // The system stores first: on a duplicate identifier the shipped
        // bundle's record wins the fold deterministically.
        alloc::vec![
            String::from(SYSTEM_COMMAND_STORE),
            String::from(SYSTEM_APPLICATION_STORE),
            String::from(INSTALLED_APP_STORE),
        ]
    };

    let mut catalog = load(store, side)?;
    let (discovered, skipped) = discover(bundles, &roots)?;
    let added = catalog
        .reconcile(&discovered)
        .map_err(|_| AppLibError::Full)?;
    if added > 0 {
        save(store, side, &catalog)?;
    }

    let message = format!("Registered {added} new application(s); skipped {skipped}.");
    let ai = format!(
        "{{\"subject\":\"program_library\",\"action\":\"rescan\",\
         \"registered\":{added},\"skipped_malformed\":{skipped},\
         \"catalog_size\":{}}}",
        catalog.len()
    );
    emit(output, "apps.library_rescan", &message, &ai);
    Ok(())
}

/// Walk `roots` breadth-first for `.app` bundle directories and read each
/// one's manifest, returning the library candidates and how many bundles
/// were skipped as unreadable or undecodable.
///
/// Listings are consumed in sorted order so the result — and therefore
/// which record wins a duplicate identifier in the fold — is deterministic.
/// A bundle directory is a sealed unit: the walk never descends into one.
/// An absent root contributes nothing; a listing refusal surfaces.
fn discover(
    bundles: &dyn Bundles,
    roots: &[String],
) -> Result<(Vec<LibraryEntry>, usize), AppLibError> {
    let mut queue: VecDeque<(String, usize)> = roots.iter().map(|root| (root.clone(), 0)).collect();
    let mut discovered = Vec::new();
    let mut visited = 0usize;
    let mut skipped = 0usize;

    while let Some((dir, depth)) = queue.pop_front() {
        let Some(mut entries) = bundles.list_dir(&dir).map_err(AppLibError::Bundle)? else {
            continue;
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for item in entries {
            visited += 1;
            if visited > MAX_WALK_ENTRIES {
                return Err(AppLibError::TreeTooLarge);
            }
            if !item.directory {
                continue;
            }
            let path = format!("{dir}/{}", item.name);
            if item.name.ends_with(BUNDLE_SUFFIX) {
                match candidate(bundles, &path) {
                    Ok(Some(entry)) => discovered.push(entry),
                    Ok(None) => {}
                    Err(()) => skipped += 1,
                }
            } else if depth + 1 < MAX_WALK_DEPTH {
                queue.push_back((path, depth + 1));
            }
        }
    }
    Ok((discovered, skipped))
}

/// Read one discovered bundle's manifest into a library candidate.
///
/// `Ok(None)` is the ordinary "not a library application" (no manifest, or
/// no library listing declared); `Err(())` is a bundle skipped fail-closed
/// (an unreadable, over-long, or undecodable manifest, or a field the
/// catalog model refuses) — counted, never aborting the scan.
fn candidate(bundles: &dyn Bundles, path: &str) -> Result<Option<LibraryEntry>, ()> {
    let bytes = match bundles.read_appinfo(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(_) => return Err(()),
    };
    let header = AppInfoHeader::from_bytes(&bytes).map_err(|_| ())?;
    let Some(category) = header.library_category() else {
        return Ok(None);
    };
    let id = EntryId::new(header.bundle_id()).map_err(|_| ())?;
    let name = DisplayName::new(header.bundle_name()).map_err(|_| ())?;
    let bundle = BundlePath::new(path).map_err(|_| ())?;
    let icon = match header.library_icon() {
        Some(asset) => Some(IconAsset::new(asset).map_err(|_| ())?),
        None => None,
    };
    Ok(Some(LibraryEntry::new(id, name, bundle, category, icon)))
}

/// The `ai` payload for an entry-change record: subject, action, the entry
/// identifier, and — when the change carries one — the entry's name,
/// folder, and bundle path, each JSON-escaped.
fn change_ai(action: &str, id: &EntryId, entry: Option<&LibraryEntry>) -> String {
    let mut ai = String::from("{\"subject\":\"program_library\",\"action\":\"");
    ai.push_str(action);
    ai.push_str("\",\"id\":");
    push_json_string(&mut ai, id.as_str());
    if let Some(entry) = entry {
        ai.push_str(",\"name\":");
        push_json_string(&mut ai, entry.name().as_str());
        ai.push_str(",\"category\":");
        push_json_string(&mut ai, entry.category().as_str());
        ai.push_str(",\"bundle\":");
        push_json_string(&mut ai, entry.bundle().as_str());
    }
    ai.push('}');
    ai
}

/// Append `text` as a JSON string literal: quoted, with `"`, `\`, and
/// control characters escaped, so a hostile-looking value can never break
/// out of the `ai` object it is embedded in.
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if (ch as u32) < 0x20 => {
                let _ = fmt::Write::write_fmt(out, format_args!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// Emit one fd-3 `stdinfo` summary record (best-effort, §20.1): terse human
/// text plus the structured `ai` object. A record that cannot be framed is
/// dropped — fd 3 is advisory and never changes the command's result.
fn emit(output: &dyn Output, code: &str, message: &str, ai: &str) {
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Summary,
        code,
        Severity::Info,
        Human::message(message),
    )
    .with_ai(ai);
    let mut buf = [0u8; 2048];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        output.info(&buf[..len]);
    }
}

#[cfg(test)]
mod tests;
