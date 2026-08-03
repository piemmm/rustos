//! Application-bundle (`.app`) manifest and loader policy — the ABI surface.
//!
//! An installed application is a `/Apps/<Name>.app/` directory with a
//! **fixed** top-level layout: a signed `AppInfo`
//! manifest, a `Run` entry-point binary, and a closed set of optional
//! sub-directories. This module owns three frozen pieces of that contract:
//!
//! * [`BundleEntry`] + [`validate_bundle_layout`] — the exact set of names a
//!   bundle may contain at its top level, and the rule that `AppInfo` and
//!   `Run` are mandatory. Anything else is a packaging defect the loader
//!   refuses.
//! * [`AppInfoHeader`] — the fixed-size, signed manifest prefix: bundle
//!   identity (id / name / version), the target ABI version and syscall
//!   table hash, the count of requested capabilities and declared MIME
//!   types, the bundle's program-library listing ([`LibraryCategory`] +
//!   optional icon asset — how the desktop's launcher discovers which
//!   folder a graphical application files under), a hash binding the
//!   signature to the bundle's contents, and the Ed25519 signer key +
//!   signature. The variable body that follows is a capability-id list
//!   (decoded by [`crate::decode_capability_ids`]) followed by the
//!   MIME-type table ([`mime_type_at`]).
//! * [`resolve_library`] — the dynamic-loader policy: a shared-library
//!   reference resolves only against the requesting bundle's own
//!   `Libraries/` directory or the curated [`SYSTEM_LIBRARIES_DIR`]; any
//!   other path is a load-time error.
//!
//! The module is `no_std`, allocation-free, and operates on borrowed byte
//! slices, so the same code runs in the kernel, in a user-space loader
//! service, and in a WebAssembly userland binary unchanged. Like every
//! `lib/abi` surface it is frozen on release: existing
//! fields, offsets, and [`BundleEntry`] names never change; new behaviour
//! ships in `abi-v2`.

use crate::le::{read_u16, read_u32};
use crate::syscall::SYSCALL_TABLE_HASH_LEN;
use crate::Errno;

/// Magic word identifying an `abi-v1` `AppInfo` manifest (`"RAI1"`
/// little-endian).
pub const APPINFO_MAGIC: u32 = u32::from_le_bytes(*b"RAI1");

/// Maximum number of capability identifiers an `AppInfo` manifest may
/// request, bounding parse work against a hostile manifest.
pub const APPINFO_MAX_CAPABILITIES: u16 = 64;

/// Maximum number of MIME / file-type associations a bundle may declare.
pub const APPINFO_MAX_MIME: u16 = 32;

/// Maximum length, in bytes, of a bundle identifier.
pub const BUNDLE_ID_MAX: usize = 64;

/// Maximum length, in bytes, of a bundle's human-readable name.
pub const BUNDLE_NAME_MAX: usize = 64;

/// Maximum length, in bytes, of a bundle version string.
pub const BUNDLE_VERSION_MAX: usize = 32;

/// Maximum length, in bytes, of one declared MIME-type string.
pub const MIME_TYPE_MAX: usize = 64;

/// Encoded length of one MIME-type body entry: a length byte plus a
/// fixed [`MIME_TYPE_MAX`] buffer.
pub const MIME_ENTRY_LEN: usize = 1 + MIME_TYPE_MAX;

/// Largest possible encoded `AppInfo` manifest: the fixed header plus the
/// maximal capability and MIME bodies. A longer file cannot be a valid
/// manifest, so every reader bounds its manifest read here and refuses a
/// bigger file before decoding a byte of it.
pub const APPINFO_WIRE_MAX: usize = AppInfoHeader::WIRE_LEN
    + APPINFO_MAX_CAPABILITIES as usize * 2
    + APPINFO_MAX_MIME as usize * MIME_ENTRY_LEN;

/// Maximum length, in bytes, of the library icon asset name a manifest may
/// declare — a plain file name inside the bundle's own `Resources/`
/// directory, so a short bound is ample and keeps the fixed header small.
pub const LIBRARY_ICON_MAX: usize = 64;

/// One of the fixed folders the desktop's program library organises
/// launchable applications into.
///
/// The set is **closed** and its spellings are locale-neutral identifiers,
/// not display text: a manifest or catalog naming anything else is refused
/// outright, so the library can never grow a free-form folder. The names
/// follow the well-understood freedesktop.org main-menu categories, so a
/// third-party packager already knows where a bundle lands. Presentation —
/// the localised folder label a launcher shows — belongs to the surface
/// that draws the folder, never to this identifier.
///
/// There is deliberately **no settings folder**: system settings are
/// reached from the system overview, not from the program library, so a
/// bundle cannot file itself among the applications a user launches.
///
/// This is manifest vocabulary: a bundle declares its folder in its signed
/// `AppInfo` ([`AppInfoHeader::library_category`]), and the program-library
/// catalog engine (`lib/proglib`) stores the same identifiers, so the two
/// can never disagree about what a folder is called.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum LibraryCategory {
    /// Calculator, text editor, clock, notes, archive tools.
    Accessories,
    /// Image viewers and editors, screenshot tools.
    Graphics,
    /// Browser, mail, chat, remote access.
    Internet,
    /// Audio and video players and recorders.
    Multimedia,
    /// Documents, spreadsheets, PDF.
    Office,
    /// Editors, IDEs, terminals used as development tools, debuggers.
    Programming,
    /// Games.
    Games,
    /// Monitors, disk tools, task shells.
    SystemTools,
    /// Small single-purpose tools that fit no folder above.
    Utilities,
    /// The catch-all a bundle that declares no category lands in.
    #[default]
    Other,
}

impl LibraryCategory {
    /// Every folder, in the order a launcher presents them.
    ///
    /// The order is the declaration order — the curated reading order of
    /// the taxonomy — and is the total order [`Ord`] derives, so a folder
    /// list and a sort of the same folders can never disagree.
    pub const ALL: [Self; 10] = [
        Self::Accessories,
        Self::Graphics,
        Self::Internet,
        Self::Multimedia,
        Self::Office,
        Self::Programming,
        Self::Games,
        Self::SystemTools,
        Self::Utilities,
        Self::Other,
    ];

    /// The canonical, locale-neutral identifier a manifest source or
    /// catalog store spells this folder with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accessories => "Accessories",
            Self::Graphics => "Graphics",
            Self::Internet => "Internet",
            Self::Multimedia => "Multimedia",
            Self::Office => "Office",
            Self::Programming => "Programming",
            Self::Games => "Games",
            Self::SystemTools => "SystemTools",
            Self::Utilities => "Utilities",
            Self::Other => "Other",
        }
    }

    /// Decode a folder identifier; `None` for anything outside the closed
    /// set.
    ///
    /// The match is exact and case-sensitive: one canonical spelling, so a
    /// rendered store has exactly one valid form and two catalogs cannot
    /// disagree over `games` and `Games`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == id)
    }

    /// The longest folder identifier, in bytes: the widest a folder value
    /// can render to, and the bound a store's line budget is derived from.
    pub const MAX_ID_LEN: usize = {
        let mut longest = 0;
        let mut index = 0;
        while index < Self::ALL.len() {
            let len = Self::ALL[index].as_str().len();
            if len > longest {
                longest = len;
            }
            index += 1;
        }
        longest
    };

    /// Encode an optional program-library listing as the manifest wire
    /// byte ([`AppInfoHeader::library`]): `0` for a bundle the library
    /// never lists, else the folder's [`Self::ALL`] position plus one.
    #[must_use]
    pub const fn to_wire(listing: Option<Self>) -> u8 {
        match listing {
            None => 0,
            Some(category) => category as u8 + 1,
        }
    }

    /// Decode the manifest wire byte: `0` is "not listed", `1..=10` name a
    /// folder, and anything else is refused — an unknown folder is a
    /// malformed manifest, never a guessed default.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a byte outside the encoding.
    pub fn from_wire(byte: u8) -> Result<Option<Self>, Errno> {
        if byte == 0 {
            return Ok(None);
        }
        let index = usize::from(byte - 1);
        Self::ALL
            .get(index)
            .copied()
            .map(Some)
            .ok_or(Errno::OutOfRange)
    }
}

impl core::fmt::Display for LibraryCategory {
    /// Renders the canonical identifier [`from_id`](Self::from_id) accepts
    /// back unchanged.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Absolute path of the curated, OS-provided shared-library directory. The dynamic loader resolves a reference against
/// this directory or the calling bundle's own `Libraries/`, and nothing
/// else.
pub const SYSTEM_LIBRARIES_DIR: &str = "/System/Libraries";

/// Absolute path of the **system app store**: the OS-provided, read-only,
/// system-signed command-app bundles (`ps.app`, `top.app`, …), each named by
/// the command it serves. The shell resolves a bare command word here
/// *before* the user's `PATH`, so a user-writable directory can never shadow
/// a system command with an attacker-supplied bundle of the same name
/// (`plans/APPS.md` §8). One definition, shared by the kernel's program
/// registry and the shell's command resolution, so the two cannot drift.
pub const SYSTEM_APP_STORE: &str = "/System/Apps";

/// Absolute path of the **user app store**: the machine-wide directory for
/// user-installed application bundles. Unlike the system app store it is
/// user-writable (never system-signed) and its bundles may be organised in
/// nested plain subdirectories; `man`'s recursive bundle search walks it —
/// after the store-then-`PATH` candidates — so an installed app's help is
/// found wherever its bundle was filed (`plans/APPS.md` §7). Launch stays
/// explicit: the shell searches it only if the user adds it to `PATH`.
pub const USER_APP_STORE: &str = "/Apps";

/// Absolute path of the **system service store**: the OS-provided,
/// read-only, system-signed service bundles (`login.app`, `devmgr.app`,
/// `sysinfod.app`, …). A service is an app: each ships as the same
/// self-contained, signed `<name>.app` bundle as a command app, discovered
/// from disk and loaded through the identical verification gate. One
/// definition, shared by the kernel's program registry, PID 1 `init`'s
/// startup config, and the image builds, so they cannot drift.
pub const SYSTEM_SERVICE_STORE: &str = "/System/Services";

/// The directory-name suffix every application bundle carries
/// (`<name>.app`). Command resolution appends it to a bare command word and
/// recognises it on an explicitly-typed bundle name (`plans/APPS.md` §9).
pub const BUNDLE_SUFFIX: &str = ".app";

/// One of the fixed set of names permitted at the top level of an
/// application bundle.
///
/// The set is closed: a bundle that contains any other top-level entry is a
/// packaging defect and the loader refuses it. `AppInfo` and `Run` are
/// files and mandatory; the rest are optional directories.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum BundleEntry {
    /// The signed manifest. Required.
    AppInfo,
    /// The entry-point `rxe` binary. Required.
    Run,
    /// Additional `rxe` binaries / plugins.
    Code,
    /// Private shared libraries used only by this bundle.
    Libraries,
    /// Images, locales, UI definitions, and other read-only assets.
    Resources,
    /// Read-only defaults copied into the user's settings on first launch.
    DefaultSettings,
    /// The internationalised help tree: one structured-Markdown document per
    /// command/topic under one directory per BCP-47 locale, plus the
    /// mandatory canonical `en-US/` source. It is the single
    /// source the `man` command, each command's short `-h`/`-?` help, and
    /// any graphical help viewer read from (`plans/APPS.md`).
    Help,
}

impl BundleEntry {
    /// Every permitted bundle entry, in canonical order.
    pub const ALL: [BundleEntry; 7] = [
        BundleEntry::AppInfo,
        BundleEntry::Run,
        BundleEntry::Code,
        BundleEntry::Libraries,
        BundleEntry::Resources,
        BundleEntry::DefaultSettings,
        BundleEntry::Help,
    ];

    /// The exact on-disk name of this entry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BundleEntry::AppInfo => "AppInfo",
            BundleEntry::Run => "Run",
            BundleEntry::Code => "Code",
            BundleEntry::Libraries => "Libraries",
            BundleEntry::Resources => "Resources",
            BundleEntry::DefaultSettings => "DefaultSettings",
            BundleEntry::Help => "Help",
        }
    }

    /// Classify a top-level entry name, or `None` if it is not one of the
    /// permitted names. The match is exact and case-sensitive.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == name)
    }

    /// `true` for the two entries every bundle must contain.
    #[must_use]
    pub const fn is_required(self) -> bool {
        matches!(self, BundleEntry::AppInfo | BundleEntry::Run)
    }

    /// `true` if this entry is a file (`AppInfo`, `Run`); the rest are
    /// directories.
    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self, BundleEntry::AppInfo | BundleEntry::Run)
    }
}

/// Why a bundle's top-level layout was rejected.
///
/// The loader fails closed: any deviation from the fixed entry set, or a
/// missing mandatory entry, refuses the whole bundle.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BundleLayoutError {
    /// A top-level entry is not one of the [`BundleEntry`] names.
    UnknownEntry,
    /// The same permitted entry appeared more than once.
    DuplicateEntry,
    /// The mandatory `AppInfo` manifest is absent.
    MissingAppInfo,
    /// The mandatory `Run` entry-point binary is absent.
    MissingRun,
}

impl core::fmt::Display for BundleLayoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::UnknownEntry => "bundle contains an entry outside the fixed layout",
            Self::DuplicateEntry => "bundle contains a duplicate top-level entry",
            Self::MissingAppInfo => "bundle is missing its AppInfo manifest",
            Self::MissingRun => "bundle is missing its Run entry point",
        };
        f.write_str(message)
    }
}

/// Validate the top-level layout of a bundle against.
///
/// `present` is the set of names found directly under the bundle root. The
/// layout is valid only if every name is a [`BundleEntry`], no name repeats,
/// and both mandatory entries (`AppInfo` and `Run`) are present.
///
/// # Errors
///
/// * [`BundleLayoutError::UnknownEntry`] for a name outside the fixed set.
/// * [`BundleLayoutError::DuplicateEntry`] if a permitted name repeats.
/// * [`BundleLayoutError::MissingAppInfo`] / [`BundleLayoutError::MissingRun`]
///   if a mandatory entry is absent.
pub fn validate_bundle_layout(present: &[&str]) -> Result<(), BundleLayoutError> {
    let mut seen = [false; BundleEntry::ALL.len()];
    for name in present {
        let entry = BundleEntry::from_name(name).ok_or(BundleLayoutError::UnknownEntry)?;
        let slot = &mut seen[entry as usize];
        if *slot {
            return Err(BundleLayoutError::DuplicateEntry);
        }
        *slot = true;
    }
    if !seen[BundleEntry::AppInfo as usize] {
        return Err(BundleLayoutError::MissingAppInfo);
    }
    if !seen[BundleEntry::Run as usize] {
        return Err(BundleLayoutError::MissingRun);
    }
    Ok(())
}

/// Which of the two policy-permitted roots a shared-library reference
/// resolved against.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LibraryScope {
    /// The reference resolved inside the requesting bundle's own
    /// `Libraries/` directory.
    Bundle,
    /// The reference resolved inside the curated [`SYSTEM_LIBRARIES_DIR`].
    System,
}

/// Why the dynamic loader refused a shared-library reference.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LibraryError {
    /// The reference, or the bundle library directory, was empty.
    Empty,
    /// The reference contained a `..` path component; a bundle may not
    /// escape its own tree or the system directory.
    Traversal,
    /// The reference points somewhere other than the bundle's `Libraries/`
    /// or [`SYSTEM_LIBRARIES_DIR`].
    OutsidePolicy,
}

impl core::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::Empty => "empty shared-library reference",
            Self::Traversal => "shared-library reference contains a '..' component",
            Self::OutsidePolicy => "shared-library reference points outside the permitted roots",
        };
        f.write_str(message)
    }
}

/// Resolve a shared-library reference under the dynamic-loader policy.
///
/// A reference is accepted only if it is an absolute path that lies inside
/// the requesting bundle's own `Libraries/` directory
/// (`bundle_libraries_dir`) or inside [`SYSTEM_LIBRARIES_DIR`], with no `..`
/// component anywhere. The bundle directory is tried first so a bundle's
/// private copy shadows the system one. The returned [`LibraryScope`]
/// records which root matched; the path itself is unchanged.
///
/// `bundle_libraries_dir` is the absolute path of the bundle's `Libraries`
/// directory (e.g. `/Apps/Example.app/Libraries`); a trailing `/` is
/// tolerated.
///
/// # Errors
///
/// * [`LibraryError::Empty`] if `reference` or `bundle_libraries_dir` is
///   empty.
/// * [`LibraryError::Traversal`] if `reference` contains a `..` component.
/// * [`LibraryError::OutsidePolicy`] if it is not inside either root.
pub fn resolve_library(
    reference: &str,
    bundle_libraries_dir: &str,
) -> Result<LibraryScope, LibraryError> {
    if reference.is_empty() || bundle_libraries_dir.is_empty() {
        return Err(LibraryError::Empty);
    }
    if has_dotdot(reference) {
        return Err(LibraryError::Traversal);
    }
    if is_within(reference, bundle_libraries_dir) {
        return Ok(LibraryScope::Bundle);
    }
    if is_within(reference, SYSTEM_LIBRARIES_DIR) {
        return Ok(LibraryScope::System);
    }
    Err(LibraryError::OutsidePolicy)
}

/// `true` if any `/`-separated component of `path` is exactly `..`.
fn has_dotdot(path: &str) -> bool {
    path.split('/').any(|component| component == "..")
}

/// `true` if `path` names a file strictly inside directory `dir`.
///
/// `dir` is normalised by stripping a single trailing `/`; `path` must then
/// begin with `dir` followed by `/` and carry a non-empty remainder, so the
/// directory itself (and a sibling whose name merely shares the prefix) is
/// not considered "within".
fn is_within(path: &str, dir: &str) -> bool {
    let dir = dir.strip_suffix('/').unwrap_or(dir);
    match path.strip_prefix(dir) {
        Some(rest) => rest.strip_prefix('/').is_some_and(|tail| !tail.is_empty()),
        None => false,
    }
}

/// Fixed-size, signed prefix of an application bundle's `AppInfo` manifest.
///
/// Field order and offsets are part of the frozen `abi-v1` surface;
/// reserved fields must be zero. The variable body that follows the header
/// is the requested capability-id list (`capability_count` little-endian
/// `u16`s, decoded by [`crate::decode_capability_ids`]) immediately followed
/// by the MIME-type table (`mime_count` entries of [`MIME_ENTRY_LEN`] bytes,
/// read by [`mime_type_at`]). The Ed25519 signature covers the **whole
/// manifest except the signature field itself**:
/// `bytes[signed_range()] ‖ bytes[WIRE_LEN..]` — the header prefix
/// concatenated with the capability/MIME body — so a tampered capability
/// request breaks the signature, not merely the signed count.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AppInfoHeader {
    /// Must equal [`APPINFO_MAGIC`].
    pub magic: u32,
    /// ABI version this bundle targets; rejected unless it equals
    /// [`crate::ABI_VERSION_CURRENT`].
    pub abi_version: u32,
    /// Implementation-defined flag bits; unknown bits must be zero.
    pub flags: u32,
    /// Number of requested capability ids in the body. Capped at
    /// [`APPINFO_MAX_CAPABILITIES`].
    pub capability_count: u16,
    /// Number of declared MIME-type associations in the body. Capped at
    /// [`APPINFO_MAX_MIME`].
    pub mime_count: u16,
    /// Valid byte count of the inline `id` buffer (`<= BUNDLE_ID_MAX`).
    pub id_len: u8,
    /// Valid byte count of the inline `name` buffer (`<= BUNDLE_NAME_MAX`).
    pub name_len: u8,
    /// Valid byte count of the inline `version` buffer
    /// (`<= BUNDLE_VERSION_MAX`).
    pub version_len: u8,
    /// Valid byte count of the inline `library_icon` buffer
    /// (`<= LIBRARY_ICON_MAX`); zero when the bundle declares no icon.
    /// Independent of `library`: the icon is the bundle's own identity,
    /// drawn wherever the bundle appears — a file-manager tile, a taskbar
    /// button, a launcher row — while `library` decides only whether the
    /// program library lists it.
    pub library_icon_len: u8,
    /// The bundle's program-library listing ([`LibraryCategory::to_wire`]):
    /// `0` for a bundle the desktop's program library never lists (every
    /// plain command app and service), else the folder it is filed under.
    pub library: u8,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved0: [u8; 3],
    /// Bundle identifier bytes; the valid prefix is `id_len` long.
    pub id: [u8; BUNDLE_ID_MAX],
    /// Human-readable name bytes; the valid prefix is `name_len` long.
    pub name: [u8; BUNDLE_NAME_MAX],
    /// Version-string bytes; the valid prefix is `version_len` long.
    pub version: [u8; BUNDLE_VERSION_MAX],
    /// Icon asset name bytes — a plain file name resolved inside the
    /// bundle's own `Resources/` directory; the valid prefix is
    /// `library_icon_len` long. All zero when the bundle declares no icon,
    /// and a draw site then falls back to its class artwork and, failing
    /// that, to the built-in glyph.
    pub library_icon: [u8; LIBRARY_ICON_MAX],
    /// SHA-256 of the kernel syscall table this bundle was linked against.
    pub syscall_table_hash: [u8; SYSCALL_TABLE_HASH_LEN],
    /// Digest binding the signature to the bundle's contents
    /// ("signature over the bundle contents").
    pub content_hash: [u8; 32],
    /// Ed25519 public key of the signer.
    pub signer_pubkey: [u8; 32],
    /// Ed25519 signature over the whole manifest except this field:
    /// `bytes[signed_range()] ‖ bytes[WIRE_LEN..]` (header prefix ‖ body).
    pub signature: [u8; 64],
}

impl AppInfoHeader {
    const OFF_CAP_COUNT: usize = 12;
    const OFF_MIME_COUNT: usize = 14;
    const OFF_ID_LEN: usize = 16;
    const OFF_NAME_LEN: usize = 17;
    const OFF_VERSION_LEN: usize = 18;
    const OFF_LIBRARY_ICON_LEN: usize = 19;
    const OFF_LIBRARY: usize = 20;
    const OFF_RESERVED0: usize = 21;
    const RESERVED0_LEN: usize = 3;
    const OFF_ID: usize = 24;
    const OFF_NAME: usize = Self::OFF_ID + BUNDLE_ID_MAX;
    const OFF_VERSION: usize = Self::OFF_NAME + BUNDLE_NAME_MAX;
    const OFF_LIBRARY_ICON: usize = Self::OFF_VERSION + BUNDLE_VERSION_MAX;
    const OFF_SYSCALL_HASH: usize = Self::OFF_LIBRARY_ICON + LIBRARY_ICON_MAX;
    const OFF_CONTENT_HASH: usize = Self::OFF_SYSCALL_HASH + SYSCALL_TABLE_HASH_LEN;
    const OFF_SIGNER: usize = Self::OFF_CONTENT_HASH + 32;
    const OFF_SIGNATURE: usize = Self::OFF_SIGNER + 32;

    /// Encoded size of an [`AppInfoHeader`] on the wire.
    pub const WIRE_LEN: usize = Self::OFF_SIGNATURE + 64;

    /// Byte range of the **header part** of the signed message — the whole
    /// header except the trailing `signature` field. The full signed
    /// message is this range concatenated with the variable body that
    /// follows the header (`bytes[signed_range()] ‖ bytes[WIRE_LEN..]`), so
    /// the capability-id list and MIME table are authenticated too; a
    /// verifier or signer that covered the header alone would leave the
    /// requested-capability ids swappable behind a valid signature.
    #[must_use]
    pub const fn signed_range() -> core::ops::Range<usize> {
        0..(Self::WIRE_LEN - 64)
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.abi_version.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[Self::OFF_CAP_COUNT..Self::OFF_CAP_COUNT + 2]
            .copy_from_slice(&self.capability_count.to_le_bytes());
        out[Self::OFF_MIME_COUNT..Self::OFF_MIME_COUNT + 2]
            .copy_from_slice(&self.mime_count.to_le_bytes());
        out[Self::OFF_ID_LEN] = self.id_len;
        out[Self::OFF_NAME_LEN] = self.name_len;
        out[Self::OFF_VERSION_LEN] = self.version_len;
        out[Self::OFF_LIBRARY_ICON_LEN] = self.library_icon_len;
        out[Self::OFF_LIBRARY] = self.library;
        out[Self::OFF_RESERVED0..Self::OFF_RESERVED0 + Self::RESERVED0_LEN]
            .copy_from_slice(&self.reserved0);
        out[Self::OFF_ID..Self::OFF_ID + BUNDLE_ID_MAX].copy_from_slice(&self.id);
        out[Self::OFF_NAME..Self::OFF_NAME + BUNDLE_NAME_MAX].copy_from_slice(&self.name);
        out[Self::OFF_VERSION..Self::OFF_VERSION + BUNDLE_VERSION_MAX]
            .copy_from_slice(&self.version);
        out[Self::OFF_LIBRARY_ICON..Self::OFF_LIBRARY_ICON + LIBRARY_ICON_MAX]
            .copy_from_slice(&self.library_icon);
        out[Self::OFF_SYSCALL_HASH..Self::OFF_SYSCALL_HASH + SYSCALL_TABLE_HASH_LEN]
            .copy_from_slice(&self.syscall_table_hash);
        out[Self::OFF_CONTENT_HASH..Self::OFF_CONTENT_HASH + 32]
            .copy_from_slice(&self.content_hash);
        out[Self::OFF_SIGNER..Self::OFF_SIGNER + 32].copy_from_slice(&self.signer_pubkey);
        out[Self::OFF_SIGNATURE..Self::OFF_SIGNATURE + 64].copy_from_slice(&self.signature);
        out
    }

    /// Decode and validate `bytes` into an [`AppInfoHeader`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, or if
    ///   `reserved0` is non-zero.
    /// * [`Errno::AbiVersionUnsupported`] if `abi_version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    /// * [`Errno::LengthOutOfRange`] if `capability_count`, `mime_count`, or
    ///   any inline string length exceeds its cap.
    /// * [`Errno::OutOfRange`] if a mandatory identity string (`id`, `name`,
    ///   `version`) is empty or is not valid UTF-8, if the `library` byte is
    ///   outside the [`LibraryCategory::from_wire`] encoding, or if the icon
    ///   bytes are not valid UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != APPINFO_MAGIC {
            return Err(Errno::BadMagic);
        }
        let abi_version = read_u32(bytes, 4);
        if abi_version != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = read_u32(bytes, 8);
        let capability_count = read_u16(bytes, Self::OFF_CAP_COUNT);
        if capability_count > APPINFO_MAX_CAPABILITIES {
            return Err(Errno::LengthOutOfRange);
        }
        let mime_count = read_u16(bytes, Self::OFF_MIME_COUNT);
        if mime_count > APPINFO_MAX_MIME {
            return Err(Errno::LengthOutOfRange);
        }
        let id_len = bytes[Self::OFF_ID_LEN];
        let name_len = bytes[Self::OFF_NAME_LEN];
        let version_len = bytes[Self::OFF_VERSION_LEN];
        let library_icon_len = bytes[Self::OFF_LIBRARY_ICON_LEN];
        let library = bytes[Self::OFF_LIBRARY];
        let mut reserved0 = [0u8; Self::RESERVED0_LEN];
        reserved0.copy_from_slice(
            &bytes[Self::OFF_RESERVED0..Self::OFF_RESERVED0 + Self::RESERVED0_LEN],
        );
        if reserved0 != [0; Self::RESERVED0_LEN] {
            return Err(Errno::BadMagic);
        }
        LibraryCategory::from_wire(library)?;

        let mut id = [0u8; BUNDLE_ID_MAX];
        id.copy_from_slice(&bytes[Self::OFF_ID..Self::OFF_ID + BUNDLE_ID_MAX]);
        let mut name = [0u8; BUNDLE_NAME_MAX];
        name.copy_from_slice(&bytes[Self::OFF_NAME..Self::OFF_NAME + BUNDLE_NAME_MAX]);
        let mut version = [0u8; BUNDLE_VERSION_MAX];
        version.copy_from_slice(&bytes[Self::OFF_VERSION..Self::OFF_VERSION + BUNDLE_VERSION_MAX]);
        let mut library_icon = [0u8; LIBRARY_ICON_MAX];
        library_icon.copy_from_slice(
            &bytes[Self::OFF_LIBRARY_ICON..Self::OFF_LIBRARY_ICON + LIBRARY_ICON_MAX],
        );
        let mut syscall_table_hash = [0u8; SYSCALL_TABLE_HASH_LEN];
        syscall_table_hash.copy_from_slice(
            &bytes[Self::OFF_SYSCALL_HASH..Self::OFF_SYSCALL_HASH + SYSCALL_TABLE_HASH_LEN],
        );
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(&bytes[Self::OFF_CONTENT_HASH..Self::OFF_CONTENT_HASH + 32]);
        let mut signer_pubkey = [0u8; 32];
        signer_pubkey.copy_from_slice(&bytes[Self::OFF_SIGNER..Self::OFF_SIGNER + 32]);
        let mut signature = [0u8; 64];
        signature.copy_from_slice(&bytes[Self::OFF_SIGNATURE..Self::OFF_SIGNATURE + 64]);

        let header = Self {
            magic,
            abi_version,
            flags,
            capability_count,
            mime_count,
            id_len,
            name_len,
            version_len,
            library_icon_len,
            library,
            reserved0,
            id,
            name,
            version,
            library_icon,
            syscall_table_hash,
            content_hash,
            signer_pubkey,
            signature,
        };
        validate_identity(header.id_len, BUNDLE_ID_MAX, &header.id)?;
        validate_identity(header.name_len, BUNDLE_NAME_MAX, &header.name)?;
        validate_identity(header.version_len, BUNDLE_VERSION_MAX, &header.version)?;
        validate_optional_text(
            header.library_icon_len,
            LIBRARY_ICON_MAX,
            &header.library_icon,
        )?;
        Ok(header)
    }

    /// The bundle identifier as text, or `""` if the inline bytes are not
    /// valid UTF-8 (which [`from_bytes`](Self::from_bytes) never produces).
    #[must_use]
    pub fn bundle_id(&self) -> &str {
        inline_str(&self.id, self.id_len)
    }

    /// The human-readable bundle name as text.
    #[must_use]
    pub fn bundle_name(&self) -> &str {
        inline_str(&self.name, self.name_len)
    }

    /// The bundle version as text.
    #[must_use]
    pub fn bundle_version(&self) -> &str {
        inline_str(&self.version, self.version_len)
    }

    /// The program-library folder this bundle asks to be listed under, or
    /// `None` for a bundle the library never lists (every plain command app
    /// and service, and a graphical application that opts out).
    #[must_use]
    pub fn library_category(&self) -> Option<LibraryCategory> {
        // `from_bytes` validated the byte; an out-of-encoding value on a
        // hand-built header reads as "not listed" rather than a guess.
        LibraryCategory::from_wire(self.library).ok().flatten()
    }

    /// The library icon asset name — a plain file name inside the bundle's
    /// own `Resources/` directory — or `None` when the bundle declares no
    /// icon and the launcher draws its class fallback.
    #[must_use]
    pub fn library_icon(&self) -> Option<&str> {
        if self.library_icon_len == 0 {
            return None;
        }
        Some(inline_str(&self.library_icon, self.library_icon_len))
    }

    /// Number of body bytes a manifest with these counts must carry: the
    /// capability list followed by the MIME-type table.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if the computation overflows `usize`.
    pub fn body_len(&self) -> Result<usize, Errno> {
        body_len(
            usize::from(self.capability_count),
            usize::from(self.mime_count),
        )
    }
}

/// Number of body bytes a manifest with `capability_count` capability ids
/// and `mime_count` MIME entries carries.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] if the computation overflows `usize`.
pub fn body_len(capability_count: usize, mime_count: usize) -> Result<usize, Errno> {
    let caps = capability_count
        .checked_mul(2)
        .ok_or(Errno::LengthOutOfRange)?;
    let mimes = mime_count
        .checked_mul(MIME_ENTRY_LEN)
        .ok_or(Errno::LengthOutOfRange)?;
    caps.checked_add(mimes).ok_or(Errno::LengthOutOfRange)
}

/// Read the `index`-th MIME-type string from a manifest `body`.
///
/// The MIME table follows the capability-id list, so `capability_count` is
/// needed to locate its base. Each entry is a length byte followed by a
/// fixed [`MIME_TYPE_MAX`] buffer; only the `len`-byte prefix is returned.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] if `body` is too short to hold the entry.
/// * [`Errno::LengthOutOfRange`] if the entry's length byte exceeds
///   [`MIME_TYPE_MAX`], or an offset computation overflows.
/// * [`Errno::OutOfRange`] if the entry's bytes are not valid UTF-8.
pub fn mime_type_at(body: &[u8], capability_count: usize, index: usize) -> Result<&str, Errno> {
    let base = capability_count
        .checked_mul(2)
        .ok_or(Errno::LengthOutOfRange)?;
    let entry = index
        .checked_mul(MIME_ENTRY_LEN)
        .and_then(|o| o.checked_add(base))
        .ok_or(Errno::LengthOutOfRange)?;
    let end = entry
        .checked_add(MIME_ENTRY_LEN)
        .ok_or(Errno::LengthOutOfRange)?;
    if body.len() < end {
        return Err(Errno::BufferTooSmall);
    }
    let len = usize::from(body[entry]);
    if len > MIME_TYPE_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    core::str::from_utf8(&body[entry + 1..entry + 1 + len]).map_err(|_| Errno::OutOfRange)
}

/// Validate one inline identity field: length within bound, non-empty, and
/// a valid UTF-8 prefix.
fn validate_identity(len: u8, max: usize, buf: &[u8]) -> Result<(), Errno> {
    let len = usize::from(len);
    if len == 0 {
        return Err(Errno::OutOfRange);
    }
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    core::str::from_utf8(&buf[..len]).map_err(|_| Errno::OutOfRange)?;
    Ok(())
}

/// Validate one optional inline text field: length within bound and a
/// valid UTF-8 prefix; empty is the ordinary "not declared".
fn validate_optional_text(len: u8, max: usize, buf: &[u8]) -> Result<(), Errno> {
    let len = usize::from(len);
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    core::str::from_utf8(&buf[..len]).map_err(|_| Errno::OutOfRange)?;
    Ok(())
}

/// Borrow the `len`-byte prefix of an inline buffer as text, or `""` if it
/// is not valid UTF-8.
fn inline_str(buf: &[u8], len: u8) -> &str {
    let len = core::cmp::min(usize::from(len), buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

/// Domain-separation prefix of the canonical bundle-contents digest
/// framing ([`digest_bundle_contents`]), so a bundle-contents hash can
/// never collide with a hash of any other TAIRiX structure.
pub const BUNDLE_CONTENT_DIGEST_MAGIC: [u8; 4] = *b"RBC1";

/// One file covered by a bundle's content digest: its bundle-root-relative
/// path (e.g. `Run`, `Help/en-US/ls.md`) and its exact bytes.
#[derive(Clone, Copy, Debug)]
pub struct BundleFileDigest<'a> {
    /// Path relative to the bundle root, `/`-separated, never `AppInfo`.
    pub path: &'a str,
    /// The file's full contents.
    pub bytes: &'a [u8],
}

/// Feed the canonical framing of a bundle's signed contents into `update`
/// — the **one** definition of what [`AppInfoHeader::content_hash`] is
/// computed over, shared by the build-time bundle composer and every
/// `BundleStore::content_hash` implementation so the two can never drift.
///
/// The digest covers **every file in the bundle except `AppInfo` itself**
/// (the manifest cannot cover its own bytes: the hash is inside it). The
/// framing is injective: a 4-byte domain magic and a little-endian `u32`
/// file count, then per file its path length (`u32` LE), path bytes, byte
/// length (`u64` LE), and bytes — so no concatenation of two different
/// file sets produces the same stream. Callers pass `files` sorted by
/// path in strictly ascending byte order; the deterministic order is what
/// makes the digest reproducible across independent store walks.
///
/// The caller owns the hash primitive (SHA-256 from `lib/crypto` in
/// production): `update` is fed the framing bytes in order, so this crate
/// stays free of any cryptographic dependency.
///
/// # Errors
///
/// Fails closed with [`Errno::OutOfRange`] — leaving the digest unusable —
/// if a path is empty, names `AppInfo`, starts or ends with `/`, contains
/// an empty, `.`, or `..` component or a NUL byte, exceeds
/// [`crate::FS_PATH_MAX`], or is not strictly greater than its
/// predecessor (unsorted or duplicate), or if the file count exceeds
/// `u32::MAX`.
pub fn digest_bundle_contents(
    files: &[BundleFileDigest<'_>],
    update: &mut dyn FnMut(&[u8]),
) -> Result<(), Errno> {
    let count = u32::try_from(files.len()).map_err(|_| Errno::OutOfRange)?;
    update(&BUNDLE_CONTENT_DIGEST_MAGIC);
    update(&count.to_le_bytes());
    let mut previous: Option<&str> = None;
    for file in files {
        validate_digest_path(file.path)?;
        if previous.is_some_and(|p| p >= file.path) {
            return Err(Errno::OutOfRange);
        }
        previous = Some(file.path);
        let path_len = u32::try_from(file.path.len()).map_err(|_| Errno::OutOfRange)?;
        update(&path_len.to_le_bytes());
        update(file.path.as_bytes());
        update(&(file.bytes.len() as u64).to_le_bytes());
        update(file.bytes);
    }
    Ok(())
}

/// Validate one covered-file path for [`digest_bundle_contents`]: rooted
/// inside the bundle (no absolute, `.`, `..`, or empty component), no NUL,
/// bounded, and never the manifest itself.
fn validate_digest_path(path: &str) -> Result<(), Errno> {
    if path.is_empty()
        || path.len() > crate::FS_PATH_MAX
        || path == BundleEntry::AppInfo.as_str()
        || path.as_bytes().contains(&0)
    {
        return Err(Errno::OutOfRange);
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(Errno::OutOfRange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        body_len, digest_bundle_contents, mime_type_at, resolve_library, validate_bundle_layout,
        AppInfoHeader, BundleEntry, BundleFileDigest, BundleLayoutError, LibraryCategory,
        LibraryError, LibraryScope, APPINFO_MAGIC, APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME,
        BUNDLE_CONTENT_DIGEST_MAGIC, BUNDLE_ID_MAX, MIME_ENTRY_LEN, MIME_TYPE_MAX,
        SYSTEM_LIBRARIES_DIR,
    };
    use crate::syscall::SYSCALL_TABLE_HASH_LEN;
    use crate::{Errno, ABI_VERSION_CURRENT};

    fn inline<const N: usize>(text: &str) -> ([u8; N], u8) {
        let mut buf = [0u8; N];
        let bytes = text.as_bytes();
        buf[..bytes.len()].copy_from_slice(bytes);
        (buf, u8::try_from(bytes.len()).expect("fits u8"))
    }

    fn sample() -> AppInfoHeader {
        let (id, id_len) = inline("com.example.editor");
        let (name, name_len) = inline("Example Editor");
        let (version, version_len) = inline("1.2.3");
        let (library_icon, library_icon_len) = inline("editor.svg");
        AppInfoHeader {
            magic: APPINFO_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: 2,
            mime_count: 1,
            id_len,
            name_len,
            version_len,
            library_icon_len,
            library: LibraryCategory::to_wire(Some(LibraryCategory::Office)),
            reserved0: [0; 3],
            id,
            name,
            version,
            library_icon,
            syscall_table_hash: [0xAB; SYSCALL_TABLE_HASH_LEN],
            content_hash: [0xCD; 32],
            signer_pubkey: [0xEF; 32],
            signature: [0x12; 64],
        }
    }

    #[test]
    fn bundle_entry_names_round_trip() {
        for entry in BundleEntry::ALL {
            assert_eq!(BundleEntry::from_name(entry.as_str()), Some(entry));
        }
        assert_eq!(BundleEntry::from_name("appinfo"), None);
        assert_eq!(BundleEntry::from_name("Plugins"), None);
        assert_eq!(BundleEntry::from_name("Documentation"), None);
        assert!(BundleEntry::AppInfo.is_required());
        assert!(BundleEntry::Run.is_required());
        assert!(!BundleEntry::Code.is_required());
        assert!(BundleEntry::AppInfo.is_file());
        assert!(!BundleEntry::Libraries.is_file());
    }

    #[test]
    fn layout_accepts_full_and_minimal() {
        assert_eq!(
            validate_bundle_layout(&[
                "AppInfo",
                "Run",
                "Code",
                "Libraries",
                "Resources",
                "DefaultSettings",
                "Help",
            ]),
            Ok(())
        );
        assert_eq!(validate_bundle_layout(&["Run", "AppInfo"]), Ok(()));
    }

    #[test]
    fn layout_rejects_unknown_duplicate_and_missing() {
        assert_eq!(
            validate_bundle_layout(&["AppInfo", "Run", "Plugins"]),
            Err(BundleLayoutError::UnknownEntry)
        );
        assert_eq!(
            validate_bundle_layout(&["AppInfo", "Run", "Run"]),
            Err(BundleLayoutError::DuplicateEntry)
        );
        assert_eq!(
            validate_bundle_layout(&["Run", "Resources"]),
            Err(BundleLayoutError::MissingAppInfo)
        );
        assert_eq!(
            validate_bundle_layout(&["AppInfo", "Resources"]),
            Err(BundleLayoutError::MissingRun)
        );
    }

    #[test]
    fn library_resolves_against_both_roots() {
        let bundle_libs = "/Apps/Example.app/Libraries";
        assert_eq!(
            resolve_library("/Apps/Example.app/Libraries/libui.so", bundle_libs),
            Ok(LibraryScope::Bundle)
        );
        assert_eq!(
            resolve_library("/System/Libraries/libtls.so", bundle_libs),
            Ok(LibraryScope::System)
        );
        // A trailing slash on the bundle directory is tolerated.
        assert_eq!(
            resolve_library(
                "/Apps/Example.app/Libraries/sub/libx.so",
                "/Apps/Example.app/Libraries/"
            ),
            Ok(LibraryScope::Bundle)
        );
    }

    #[test]
    fn library_refuses_outside_policy_and_traversal() {
        let bundle_libs = "/Apps/Example.app/Libraries";
        assert_eq!(
            resolve_library("/System/Kernel/secret", bundle_libs),
            Err(LibraryError::OutsidePolicy)
        );
        // A sibling that merely shares the prefix is not "within".
        assert_eq!(
            resolve_library("/Apps/Example.app/LibrariesEvil/x.so", bundle_libs),
            Err(LibraryError::OutsidePolicy)
        );
        assert_eq!(
            resolve_library("/System/Libraries/../Kernel/x", bundle_libs),
            Err(LibraryError::Traversal)
        );
        assert_eq!(resolve_library("", bundle_libs), Err(LibraryError::Empty));
        assert_eq!(
            resolve_library("/System/Libraries/x", ""),
            Err(LibraryError::Empty)
        );
        // The directory itself, with no file component, is not within.
        assert_eq!(
            resolve_library(SYSTEM_LIBRARIES_DIR, bundle_libs),
            Err(LibraryError::OutsidePolicy)
        );
    }

    #[test]
    fn header_wire_size_is_frozen() {
        assert_eq!(AppInfoHeader::WIRE_LEN, 408);
        assert_eq!(
            AppInfoHeader::WIRE_LEN,
            core::mem::size_of::<AppInfoHeader>()
        );
    }

    #[test]
    fn header_round_trips() {
        let h = sample();
        let bytes = h.to_le_bytes();
        let decoded = AppInfoHeader::from_bytes(&bytes).expect("valid");
        assert_eq!(decoded, h);
        assert_eq!(decoded.bundle_id(), "com.example.editor");
        assert_eq!(decoded.bundle_name(), "Example Editor");
        assert_eq!(decoded.bundle_version(), "1.2.3");
        assert_eq!(decoded.library_category(), Some(LibraryCategory::Office));
        assert_eq!(decoded.library_icon(), Some("editor.svg"));
    }

    #[test]
    fn an_unlisted_header_reads_no_category_and_no_icon() {
        let mut h = sample();
        h.library = LibraryCategory::to_wire(None);
        h.library_icon = [0; super::LIBRARY_ICON_MAX];
        h.library_icon_len = 0;
        let decoded = AppInfoHeader::from_bytes(&h.to_le_bytes()).expect("valid");
        assert_eq!(decoded.library_category(), None);
        assert_eq!(decoded.library_icon(), None);
    }

    /// A bundle's icon is its own identity, not a property of the launcher:
    /// every command app declares one and none of them is listed in the
    /// program library, so an icon without a category must decode.
    #[test]
    fn an_unlisted_bundle_may_still_declare_its_own_icon() {
        let mut h = sample();
        h.library = LibraryCategory::to_wire(None);
        let decoded = AppInfoHeader::from_bytes(&h.to_le_bytes()).expect("valid");
        assert_eq!(decoded.library_category(), None);
        assert_eq!(decoded.library_icon(), Some("editor.svg"));
    }

    #[test]
    fn every_library_category_round_trips_the_wire_byte() {
        assert_eq!(LibraryCategory::from_wire(0), Ok(None));
        for category in LibraryCategory::ALL {
            let byte = LibraryCategory::to_wire(Some(category));
            assert_eq!(LibraryCategory::from_wire(byte), Ok(Some(category)));
        }
        let beyond = u8::try_from(LibraryCategory::ALL.len()).expect("small set") + 1;
        assert_eq!(LibraryCategory::from_wire(beyond), Err(Errno::OutOfRange));
        assert_eq!(LibraryCategory::from_wire(0xFF), Err(Errno::OutOfRange));
    }

    #[test]
    fn every_library_identifier_round_trips_the_decoder() {
        for category in LibraryCategory::ALL {
            assert_eq!(LibraryCategory::from_id(category.as_str()), Some(category));
        }
        for hostile in ["games", "GAMES", "Settings", "", " Games", "Games "] {
            assert_eq!(LibraryCategory::from_id(hostile), None, "{hostile:?}");
        }
        assert_eq!(LibraryCategory::default(), LibraryCategory::Other);
        let mut sorted = LibraryCategory::ALL;
        sorted.sort_unstable();
        assert_eq!(sorted, LibraryCategory::ALL, "presentation order is Ord");
        assert!(LibraryCategory::ALL
            .into_iter()
            .all(|c| c.as_str().len() <= LibraryCategory::MAX_ID_LEN));
        assert!(LibraryCategory::ALL
            .into_iter()
            .any(|c| c.as_str().len() == LibraryCategory::MAX_ID_LEN));
    }

    #[test]
    fn header_rejects_a_malformed_library_listing() {
        // A library byte outside the encoding.
        let mut h = sample();
        h.library = u8::try_from(LibraryCategory::ALL.len()).expect("small set") + 1;
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::OutOfRange)
        );

        // An over-long icon length.
        let mut h = sample();
        h.library_icon_len = u8::try_from(super::LIBRARY_ICON_MAX + 1).unwrap();
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );

        // Non-UTF-8 icon bytes.
        let mut h = sample();
        h.library_icon[0] = 0xFF;
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn signed_range_excludes_signature() {
        let range = AppInfoHeader::signed_range();
        assert_eq!(range.start, 0);
        assert_eq!(range.end, AppInfoHeader::WIRE_LEN - 64);
    }

    #[test]
    fn header_rejects_short() {
        assert_eq!(
            AppInfoHeader::from_bytes(&[0u8; 16]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn header_rejects_bad_magic_and_version_and_reserved() {
        let mut bytes = sample().to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(AppInfoHeader::from_bytes(&bytes), Err(Errno::BadMagic));

        let mut h = sample();
        h.abi_version = 99;
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::AbiVersionUnsupported)
        );

        let mut h = sample();
        h.reserved0 = [0, 1, 0];
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn header_rejects_excess_counts() {
        let mut h = sample();
        h.capability_count = APPINFO_MAX_CAPABILITIES + 1;
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
        let mut h = sample();
        h.mime_count = APPINFO_MAX_MIME + 1;
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn header_rejects_empty_or_overlong_or_nonutf8_identity() {
        let mut h = sample();
        h.id_len = 0;
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::OutOfRange)
        );

        let mut h = sample();
        // exceeds the name bound too
        h.name_len = u8::try_from(BUNDLE_ID_MAX + 1).unwrap();
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );

        let mut h = sample();
        h.id[0] = 0xFF; // invalid UTF-8 lead byte
        assert_eq!(
            AppInfoHeader::from_bytes(&h.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn body_len_and_mime_read() {
        // 2 caps + 1 mime entry.
        assert_eq!(body_len(2, 1), Ok(4 + MIME_ENTRY_LEN));
        // Build a body: two cap ids then one mime entry "text/plain".
        let mut body = [0u8; 4 + MIME_ENTRY_LEN];
        let mime = b"text/plain";
        body[4] = u8::try_from(mime.len()).unwrap();
        body[5..5 + mime.len()].copy_from_slice(mime);
        assert_eq!(mime_type_at(&body, 2, 0), Ok("text/plain"));
        // Out-of-range index is a short buffer.
        assert_eq!(mime_type_at(&body, 2, 1), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn mime_read_rejects_overlong_and_nonutf8() {
        let mut body = [0u8; MIME_ENTRY_LEN];
        body[0] = u8::try_from(MIME_TYPE_MAX + 1).unwrap();
        assert_eq!(mime_type_at(&body, 0, 0), Err(Errno::LengthOutOfRange));

        let mut body = [0u8; MIME_ENTRY_LEN];
        body[0] = 1;
        body[1] = 0xFF;
        assert_eq!(mime_type_at(&body, 0, 0), Err(Errno::OutOfRange));
    }

    /// Collect a digest framing into a fixed buffer for byte-exact
    /// assertions (the crate is `no_std`, so no `Vec` here).
    fn collect_framing<'a>(
        files: &[BundleFileDigest<'_>],
        buf: &'a mut [u8],
    ) -> Result<&'a [u8], Errno> {
        let mut used = 0usize;
        digest_bundle_contents(files, &mut |chunk| {
            buf[used..used + chunk.len()].copy_from_slice(chunk);
            used += chunk.len();
        })?;
        Ok(&buf[..used])
    }

    #[test]
    fn digest_framing_is_exact_and_ordered() {
        let files = [
            BundleFileDigest {
                path: "Help/en-US/ls.md",
                bytes: b"doc",
            },
            BundleFileDigest {
                path: "Run",
                bytes: b"rxe",
            },
        ];
        let mut buf = [0u8; 128];
        let framed = collect_framing(&files, &mut buf).expect("valid file set");

        let mut expected = [0u8; 128];
        let mut at = 0usize;
        let mut put = |chunk: &[u8]| {
            expected[at..at + chunk.len()].copy_from_slice(chunk);
            at += chunk.len();
        };
        put(&BUNDLE_CONTENT_DIGEST_MAGIC);
        put(&2u32.to_le_bytes());
        put(&(16u32).to_le_bytes());
        put(b"Help/en-US/ls.md");
        put(&3u64.to_le_bytes());
        put(b"doc");
        put(&(3u32).to_le_bytes());
        put(b"Run");
        put(&3u64.to_le_bytes());
        put(b"rxe");
        assert_eq!(framed, &expected[..at]);
    }

    #[test]
    fn digest_distinguishes_boundary_shifts() {
        // Same concatenated bytes, different file split: the length framing
        // must produce different streams.
        let a = [
            BundleFileDigest {
                path: "Run",
                bytes: b"ab",
            },
            BundleFileDigest {
                path: "Runb",
                bytes: b"",
            },
        ];
        let b = [
            BundleFileDigest {
                path: "Run",
                bytes: b"a",
            },
            BundleFileDigest {
                path: "Runb",
                bytes: b"b",
            },
        ];
        let mut buf_a = [0u8; 64];
        let mut buf_b = [0u8; 64];
        let framed_a = collect_framing(&a, &mut buf_a).expect("valid");
        let framed_b = collect_framing(&b, &mut buf_b).expect("valid");
        assert_ne!(framed_a, framed_b);
    }

    #[test]
    fn digest_rejects_unsorted_duplicate_and_bad_paths() {
        let mut sink = |_: &[u8]| {};
        // Unsorted.
        let unsorted = [
            BundleFileDigest {
                path: "Run",
                bytes: b"",
            },
            BundleFileDigest {
                path: "Help/en-US/ls.md",
                bytes: b"",
            },
        ];
        assert_eq!(
            digest_bundle_contents(&unsorted, &mut sink),
            Err(Errno::OutOfRange)
        );
        // Duplicate.
        let duplicate = [
            BundleFileDigest {
                path: "Run",
                bytes: b"",
            },
            BundleFileDigest {
                path: "Run",
                bytes: b"",
            },
        ];
        assert_eq!(
            digest_bundle_contents(&duplicate, &mut sink),
            Err(Errno::OutOfRange)
        );
        // The manifest itself, absolute/parent/empty components, and NUL.
        for path in [
            "AppInfo",
            "",
            "/Run",
            "Run/",
            "Help//x.md",
            "Help/../Run",
            "Help/./x.md",
            "Run\0",
        ] {
            let files = [BundleFileDigest { path, bytes: b"" }];
            assert_eq!(
                digest_bundle_contents(&files, &mut sink),
                Err(Errno::OutOfRange),
                "path {path:?} must be refused"
            );
        }
        // A nested AppInfo-named file inside a directory is a different
        // path from the bundle manifest and is legitimately covered.
        let nested = [BundleFileDigest {
            path: "Resources/AppInfo",
            bytes: b"",
        }];
        assert_eq!(digest_bundle_contents(&nested, &mut sink), Ok(()));
    }

    #[test]
    fn digest_of_no_files_is_the_empty_frame() {
        let mut buf = [0u8; 8];
        let framed = collect_framing(&[], &mut buf).expect("empty set is valid");
        let mut expected = [0u8; 8];
        expected[..4].copy_from_slice(&BUNDLE_CONTENT_DIGEST_MAGIC);
        assert_eq!(framed, &expected[..8]);
    }
}
