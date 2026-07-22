//! TAIRiX shared filesystem path-spelling parser (`lib/path`).
//!
//! TAIRiX storage is a *forest of named roots*, not one global Unix tree, so a
//! path string names its root explicitly. Several components need to turn such
//! a string into a structured, validated form — the shell first (`cd`, prompt
//! display, word and tilde expansion, completion), and later the file browser
//! and file-management tools. That lexing and normalisation is *identical*
//! wherever it happens, so it lives here once and every consumer imports it,
//! rather than each growing a private path parser.
//!
//! This crate is a pure *spelling* step: it turns a string into a typed
//! [`Path`] (a [`Root`] plus normalised components). It does **not** resolve an
//! alias to a volume, open anything, or perform a capability check; a resolved
//! name is still subject to inode permissions, ACLs, capability gates, mount
//! flags, and MAC policy at open time. Resolving a root handle and opening a
//! descriptor are owned by the storage/filesystem subsystems, not this layer,
//! so parsing a string here can never widen authority.
//!
//! # Accepted spellings
//!
//! - **Synthetic view** — a leading `/`, e.g. `/Users/ian/Documents`. The `/`
//!   view is a convenience surface assembled from aliases; it is never the
//!   canonical identity of storage. The root is [`Root::View`].
//! - **Alias shorthand** — `Alias:/path`, e.g. `Home:/Documents/spec.md`. The
//!   first `/` after the `:` marks the root boundary; the alias name is not a
//!   path component. The root is [`Root::Alias`].
//! - **Expanded alias** — `alias::Name/path`, e.g. `alias::Home/Documents`. The
//!   normalised internal spelling of the same thing; it parses to the identical
//!   [`Root::Alias`].
//! - **Relative** — no leading `/` and no leading `Name:`/`Name::` token, e.g.
//!   `Documents/spec.md`, `.`, or `../notes`. The root is [`Root::Relative`];
//!   the caller resolves it against a current directory.
//! - **Durable volume identity** — `id::<volume-id>/path`, e.g.
//!   `id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/Documents`. The volume id is
//!   the canonical hyphenated lowercase UUID spelling (`8-4-4-4-12` hex
//!   digits); any other spelling is refused, so a rendered path always
//!   re-parses to the same value. The root is [`Root::VolumeId`]. This is
//!   the durable machine form (`docs/src/filesystem/drives.md` §8): it
//!   names a volume root by its stable identity, independent of any alias
//!   or the `/` view, and is resolved by the kernel's volume forest.
//!
//! A leading `Name:` (single colon) that is **not** followed by `/` is refused
//! with [`PathError::NotAPath`]: that is either a resource reference
//! (`namespace:selector`, owned by the separate resource-reference grammar) or
//! an alias path missing its `/` (which TAIRiX deliberately does not treat as a
//! relative path — there is no Windows-style `Drive:relative`). A caller that
//! also handles resource references routes a `NotAPath` string to that resolver.
//!
//! The remaining administrative resolver spellings — `fs::<driver>/<root>/…`,
//! the `<driver>::<root>/…` shorthand, `dev::…`, and `net::…` — are refused
//! with [`PathError::UnsupportedResolver`]. They belong to recovery and
//! diagnostic tooling that does not exist yet; parsing them now, with no
//! consumer, would be a speculative interface. They are added by the stage
//! that introduces their callers (`id::` landed with the kernel volume
//! forest, `plans/DEVICES.md` D3a).
//!
//! # Bounds and fail-closed behaviour
//!
//! A path string is untrusted input, so every dimension is a fixed security
//! bound, not a growable capacity: [`MAX_PATH_LEN`], [`MAX_COMPONENTS`],
//! [`MAX_COMPONENT_LEN`], and [`MAX_ALIAS_LEN`]. A path that exceeds a bound,
//! contains an empty interior component (`a//b`), carries a control character,
//! contains a `:` inside a component (the colon is a structural delimiter, so a
//! rendered path always re-parses to the same value), or uses `..` to climb
//! above a rooted path is rejected by [`parse`] with a typed [`PathError`] — it
//! is never silently "fixed up". `..` cannot escape
//! above an alias or view root ([`PathError::EscapesRoot`]); a leading `..` in
//! a *relative* path is preserved for the caller to resolve.
//!
//! Parsing is the only fallible step and it never panics: [`parse`] always
//! returns a [`Path`] or a [`PathError`]. Every operation runs in time linear
//! in the input length, with no recursion, so neither a hostile path nor a
//! hostile component can trigger runaway work.
//!
//! # Example
//!
//! ```
//! use tairix_path::{parse, Root};
//!
//! let p = parse("Home:/Documents/../Photos/2026").unwrap();
//! assert_eq!(p.root(), &Root::Alias("Home".into()));
//! assert_eq!(p.components(), &["Photos", "2026"]);
//! assert_eq!(p.to_string(), "Home:/Photos/2026");
//!
//! // The expanded spelling parses to the identical value.
//! assert_eq!(parse("alias::Home/Photos/2026").unwrap().to_string(), "Home:/Photos/2026");
//!
//! // A view path and a relative path.
//! assert_eq!(parse("/System/Kernel").unwrap().to_string(), "/System/Kernel");
//! assert_eq!(parse("../notes").unwrap().to_string(), "../notes");
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

/// Largest accepted path string, in bytes. Longer input is rejected outright,
/// before any per-component work, so an over-long string cannot drive cost.
pub const MAX_PATH_LEN: usize = 4096;

/// Largest number of components a normalised path may contain.
pub const MAX_COMPONENTS: usize = 256;

/// Largest single path component, in bytes.
pub const MAX_COMPONENT_LEN: usize = 255;

/// Largest alias name, in bytes.
pub const MAX_ALIAS_LEN: usize = 64;

/// Exact byte length of a canonical volume-id spelling
/// (`8-4-4-4-12` lowercase hex, e.g.
/// `b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001`).
pub const VOLUME_ID_TEXT_LEN: usize = 36;

/// A stable volume identity, parsed from the canonical hyphenated
/// lowercase UUID spelling of an `id::`-rooted path.
///
/// The 16 raw bytes are the UUID's hex digits in text order (big-endian
/// field order, exactly as spelled). This is the parse-time *spelling*
/// value; resolving it to a live volume root is the kernel volume
/// forest's job, so holding one grants nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VolumeId([u8; 16]);

impl VolumeId {
    /// Wrap 16 raw identity bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 raw identity bytes, in spelling order.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for VolumeId {
    /// Renders the canonical hyphenated lowercase spelling, which
    /// [`parse`] accepts back unchanged.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The root a [`Path`] is anchored to — how its components are to be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Root {
    /// The synthetic session view (`/…`): a convenience tree assembled from
    /// aliases, never the canonical identity of storage.
    View,
    /// A named alias root (`Alias:/…` or `alias::Alias/…`). The stored string
    /// is the validated alias name, without the `:`/`::` delimiter.
    Alias(String),
    /// A durable volume-identity root (`id::<volume-id>/…`): the stable
    /// machine form that names a volume root independently of any alias or
    /// the `/` view (`docs/src/filesystem/drives.md` §8).
    VolumeId(VolumeId),
    /// A relative path (`…`), resolved by the caller against a current
    /// directory. The components may begin with `..` entries.
    Relative,
}

/// A parsed, normalised TAIRiX path: a [`Root`] plus its components.
///
/// Components never contain `/`, are never empty, and — for a [`Root::View`],
/// [`Root::Alias`], or [`Root::VolumeId`] — never contain a `.` or `..`
/// navigation entry (those are resolved away, and a `..` that would climb
/// above the root is a parse error). A [`Root::Relative`] path may retain
/// leading `..` entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    root: Root,
    components: Vec<String>,
}

impl Path {
    /// The root this path is anchored to.
    #[must_use]
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// The normalised path components, outermost first.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The alias name, when this path is anchored to an alias root.
    #[must_use]
    pub fn alias(&self) -> Option<&str> {
        match &self.root {
            Root::Alias(name) => Some(name),
            Root::View | Root::VolumeId(_) | Root::Relative => None,
        }
    }

    /// The volume identity, when this path is anchored to an `id::` root.
    #[must_use]
    pub fn volume_id(&self) -> Option<&VolumeId> {
        match &self.root {
            Root::VolumeId(id) => Some(id),
            Root::View | Root::Alias(_) | Root::Relative => None,
        }
    }

    /// Whether this path names its own root (a view, alias, or volume-id
    /// path), as opposed to a relative path resolved against a current
    /// directory.
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        matches!(self.root, Root::View | Root::Alias(_) | Root::VolumeId(_))
    }

    /// The canonical spelling of this path truncated to its first `len`
    /// components, or `None` when `len` exceeds the component count.
    ///
    /// This is the one definition of the ancestor walk the parent-creating
    /// and parent-removing tools share: `mkdir -p` spells each prefix
    /// outermost-first (`prefix(1)`, `prefix(2)`, …) and `rmdir -p` each
    /// proper ancestor innermost-first, so neither tool re-derives how a
    /// truncated path is spelled. `prefix(0)` is the bare root (`/`,
    /// `Name:/`, `id::<volume-id>/`, or `.` for a relative path).
    #[must_use]
    pub fn prefix(&self, len: usize) -> Option<String> {
        if len > self.components.len() {
            return None;
        }
        Some(
            Self {
                root: self.root.clone(),
                components: self.components[..len].to_vec(),
            }
            .to_string(),
        )
    }
}

impl fmt::Display for Path {
    /// Renders the canonical human spelling: `/a/b` for a view, `Name:/a/b`
    /// for an alias, `id::<volume-id>/a/b` for a volume-id root, and `a/b`
    /// (or `.` when empty) for a relative path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Root::View => {
                f.write_str("/")?;
                write_components(f, &self.components)
            }
            Root::Alias(name) => {
                write!(f, "{name}:/")?;
                write_components(f, &self.components)
            }
            Root::VolumeId(id) => {
                write!(f, "id::{id}/")?;
                write_components(f, &self.components)
            }
            Root::Relative => {
                if self.components.is_empty() {
                    f.write_str(".")
                } else {
                    write_components(f, &self.components)
                }
            }
        }
    }
}

fn write_components(f: &mut fmt::Formatter<'_>, components: &[String]) -> fmt::Result {
    let mut first = true;
    for c in components {
        if first {
            first = false;
        } else {
            f.write_str("/")?;
        }
        f.write_str(c)?;
    }
    Ok(())
}

/// Why a path string was rejected. Parsing fails closed: on any of these the
/// caller has no `Path`, never a partially-applied or guessed one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The input was empty.
    Empty,
    /// The input exceeded [`MAX_PATH_LEN`] bytes.
    TooLong,
    /// The normalised path exceeded [`MAX_COMPONENTS`] components.
    TooManyComponents,
    /// A single component exceeded [`MAX_COMPONENT_LEN`] bytes.
    ComponentTooLong,
    /// An empty interior component (`a//b`, or a leading `//`).
    EmptyComponent,
    /// A component contained a control character (including NUL).
    ControlCharacter,
    /// A component contained a `:`. The colon is a structural delimiter in the
    /// path grammar, so it is never a literal filename character; a name that
    /// needs one is escaped by the filesystem driver, not spelled here.
    ColonInComponent,
    /// A single leaf name (a rename target, a new-folder name) was `.` or
    /// `..`: the navigation names, never a real file or directory name.
    /// Only reported by [`validate_file_name`]; the path parser normalises
    /// `.`/`..` rather than rejecting them.
    ReservedName,
    /// A single leaf name contained a `/`: it names a path, not one file or
    /// directory. Only reported by [`validate_file_name`].
    SeparatorInName,
    /// A `..` component tried to climb above a view, alias, or volume-id
    /// root.
    EscapesRoot,
    /// The alias name was empty (`:/…` or `alias::/…`).
    AliasEmpty,
    /// The alias name exceeded [`MAX_ALIAS_LEN`] bytes.
    AliasTooLong,
    /// The alias name contained a character that is not allowed in an alias
    /// (a control character, `/`, or `:`).
    AliasInvalidChar,
    /// A resolver token was empty (`::…`).
    ResolverEmpty,
    /// An `id::` volume identity was not the canonical hyphenated lowercase
    /// UUID spelling (`8-4-4-4-12` hex digits).
    VolumeIdInvalid,
    /// A resolver-prefixed spelling (`fs::`, `dev::`, `net::`, or a
    /// `<driver>::` shorthand) was used. These administrative forms have no
    /// consumer yet and are not parsed by this layer.
    UnsupportedResolver,
    /// The input had a `Name:selector` shape (single colon not followed by
    /// `/`): a resource reference owned by the separate resource-reference
    /// grammar, or an alias path missing its `/`. It is not a filesystem path.
    NotAPath,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            PathError::Empty => "empty path",
            PathError::TooLong => "path exceeds the maximum length",
            PathError::TooManyComponents => "path has too many components",
            PathError::ComponentTooLong => "path component exceeds the maximum length",
            PathError::EmptyComponent => "path has an empty component",
            PathError::ControlCharacter => "path component contains a control character",
            PathError::ColonInComponent => "path component contains a `:` (a reserved delimiter)",
            PathError::ReservedName => "`.` and `..` are not valid file or directory names",
            PathError::SeparatorInName => "a file or directory name may not contain `/`",
            PathError::EscapesRoot => "path climbs above its root",
            PathError::AliasEmpty => "alias name is empty",
            PathError::AliasTooLong => "alias name exceeds the maximum length",
            PathError::AliasInvalidChar => "alias name contains an invalid character",
            PathError::ResolverEmpty => "resolver name is empty",
            PathError::VolumeIdInvalid => {
                "volume id is not the canonical hyphenated lowercase UUID spelling"
            }
            PathError::UnsupportedResolver => {
                "resolver-prefixed path spelling is not supported here"
            }
            PathError::NotAPath => "input is a resource reference or an alias path missing its `/`",
        };
        f.write_str(msg)
    }
}

/// Parse and normalise a TAIRiX path string into a typed [`Path`].
///
/// See the crate documentation for the accepted spellings, the fixed security
/// bounds, and the fail-closed rules. This is the only fallible step; the
/// returned [`Path`] can be displayed and inspected infallibly.
pub fn parse(input: &str) -> Result<Path, PathError> {
    if input.is_empty() {
        return Err(PathError::Empty);
    }
    if input.len() > MAX_PATH_LEN {
        return Err(PathError::TooLong);
    }

    // A leading `/` is a synthetic-view path, regardless of any later `:`.
    if let Some(rest) = input.strip_prefix('/') {
        let components = normalise(rest, true)?;
        return Ok(Path {
            root: Root::View,
            components,
        });
    }

    if let Some(colon) = root_delimiter(input) {
        return parse_rooted(input, colon);
    }

    // No leading root token: a relative path.
    let components = normalise(input, false)?;
    Ok(Path {
        root: Root::Relative,
        components,
    })
}

/// The byte offset of the root-delimiting `:`, if `input` carries one.
///
/// A `:` before the first `/` is a root delimiter (an alias shorthand or a
/// resolver token). A `:` that appears only *after* a `/` is an ordinary
/// character inside a relative path component and is not a delimiter.
fn root_delimiter(input: &str) -> Option<usize> {
    match (input.find(':'), input.find('/')) {
        (Some(c), Some(s)) if c < s => Some(c),
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// The byte length of a leading alias-root prefix (`Name:/`), if `input`
/// begins with one — the offset of the first byte after the `/`.
///
/// This is the grammar's own root-delimiter rule (a `:` before the first
/// `/`, followed by `/`, naming a valid alias), exposed for *lexical* text
/// tools — `basename`/`dirname`-style string surgery that must treat
/// `Name:/` the way POSIX treats `/` (a root that is never stripped into)
/// without resolving or normalising the path. It validates only the alias
/// name; the remainder of the string is untouched, so input `parse` would
/// reject (an interior `//`, a control character in a component) still
/// reports its prefix here. Resolver spellings (`Name::…`) and resource
/// references (`Name:selector`) have no `Name:/` prefix and return `None`.
#[must_use]
pub fn alias_root_len(input: &str) -> Option<usize> {
    let colon = root_delimiter(input)?;
    if input.as_bytes().get(colon + 1) != Some(&b'/') {
        return None;
    }
    validate_alias(&input[..colon]).ok()?;
    Some(colon + 2)
}

/// Parse a path whose first `:` (at byte offset `colon`) is a root delimiter.
fn parse_rooted(input: &str, colon: usize) -> Result<Path, PathError> {
    let after_colon = &input[colon + 1..];

    // `Name::rest` — a resolver-prefixed spelling.
    if let Some(rest) = after_colon.strip_prefix(':') {
        let resolver = &input[..colon];
        if resolver.is_empty() {
            return Err(PathError::ResolverEmpty);
        }
        if resolver == "alias" {
            return parse_expanded_alias(rest);
        }
        if resolver == "id" {
            return parse_volume_id(rest);
        }
        return Err(PathError::UnsupportedResolver);
    }

    // `Name:/path` — an alias shorthand. A single colon not followed by `/` is
    // a resource reference or a missing-slash alias path, neither of which is a
    // filesystem path.
    let Some(rest) = after_colon.strip_prefix('/') else {
        return Err(PathError::NotAPath);
    };
    let alias = validate_alias(&input[..colon])?;
    let components = normalise(rest, true)?;
    Ok(Path {
        root: Root::Alias(alias),
        components,
    })
}

/// Parse the tail of an expanded alias spelling (`alias::` already stripped),
/// i.e. `Name` or `Name/path/inside/root`.
fn parse_expanded_alias(rest: &str) -> Result<Path, PathError> {
    let (name, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash + 1..]),
        None => (rest, ""),
    };
    let alias = validate_alias(name)?;
    let components = normalise(path, true)?;
    Ok(Path {
        root: Root::Alias(alias),
        components,
    })
}

/// Parse the tail of a volume-identity spelling (`id::` already stripped),
/// i.e. `<volume-id>` or `<volume-id>/path/inside/root`.
fn parse_volume_id(rest: &str) -> Result<Path, PathError> {
    let (text, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash + 1..]),
        None => (rest, ""),
    };
    let id = validate_volume_id(text)?;
    let components = normalise(path, true)?;
    Ok(Path {
        root: Root::VolumeId(id),
        components,
    })
}

/// Validate the canonical hyphenated lowercase UUID spelling and decode its
/// 16 identity bytes. Any other spelling — wrong length, misplaced hyphen,
/// uppercase or non-hex digit — is refused, never "fixed up", so a rendered
/// path always re-parses to the same value.
fn validate_volume_id(text: &str) -> Result<VolumeId, PathError> {
    let bytes = text.as_bytes();
    if bytes.len() != VOLUME_ID_TEXT_LEN {
        return Err(PathError::VolumeIdInvalid);
    }
    let mut id = [0u8; 16];
    let mut out = 0;
    let mut high: Option<u8> = None;
    for (index, &byte) in bytes.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return Err(PathError::VolumeIdInvalid);
            }
            continue;
        }
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => return Err(PathError::VolumeIdInvalid),
        };
        match high.take() {
            None => high = Some(digit),
            Some(h) => {
                id[out] = (h << 4) | digit;
                out += 1;
            }
        }
    }
    Ok(VolumeId::from_bytes(id))
}

/// Validate an alias name and return it as an owned string.
fn validate_alias(name: &str) -> Result<String, PathError> {
    if name.is_empty() {
        return Err(PathError::AliasEmpty);
    }
    if name.len() > MAX_ALIAS_LEN {
        return Err(PathError::AliasTooLong);
    }
    for ch in name.chars() {
        if ch.is_control() || ch == '/' || ch == ':' {
            return Err(PathError::AliasInvalidChar);
        }
    }
    Ok(String::from(name))
}

/// Split `path` on `/`, validate and normalise its components (`.` dropped,
/// `..` applied), and return the resulting component list.
///
/// `rooted` selects the `..` policy: a rooted (view/alias) path may not climb
/// above its root, whereas a relative path preserves leading `..` entries for
/// the caller to resolve.
fn normalise(path: &str, rooted: bool) -> Result<Vec<String>, PathError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }

    let mut components: Vec<String> = Vec::new();
    let pieces: Vec<&str> = path.split('/').collect();
    let last = pieces.len() - 1;

    for (index, piece) in pieces.iter().enumerate() {
        if piece.is_empty() {
            // A trailing `/` (an empty final piece) is accepted and ignored;
            // an empty interior piece is a `//` and is rejected.
            if index == last {
                continue;
            }
            return Err(PathError::EmptyComponent);
        }
        match *piece {
            "." => {}
            ".." => {
                if let Some(top) = components.last() {
                    if top == ".." {
                        push_component(&mut components, "..")?;
                    } else {
                        components.pop();
                    }
                } else if rooted {
                    return Err(PathError::EscapesRoot);
                } else {
                    push_component(&mut components, "..")?;
                }
            }
            component => {
                validate_component(component)?;
                push_component(&mut components, component)?;
            }
        }
    }

    Ok(components)
}

/// Push a validated component, enforcing the [`MAX_COMPONENTS`] bound.
fn push_component(components: &mut Vec<String>, component: &str) -> Result<(), PathError> {
    if components.len() >= MAX_COMPONENTS {
        return Err(PathError::TooManyComponents);
    }
    components.push(String::from(component));
    Ok(())
}

/// Validate a single ordinary path component (not `.` or `..`).
fn validate_component(component: &str) -> Result<(), PathError> {
    if component.len() > MAX_COMPONENT_LEN {
        return Err(PathError::ComponentTooLong);
    }
    if component.chars().any(char::is_control) {
        return Err(PathError::ControlCharacter);
    }
    // The colon is a structural delimiter (alias/resolver/reference), never a
    // literal filename character, so a rendered path always re-parses to the
    // same value.
    if component.contains(':') {
        return Err(PathError::ColonInComponent);
    }
    Ok(())
}

/// Validate a single filesystem **leaf name** — one file or directory name,
/// not a path.
///
/// A valid leaf name is exactly one ordinary path component: it is non-empty,
/// is neither `.` nor `..`, contains no `/` separator, no control character
/// (so no NUL), and no `:` (the structural delimiter), and is within
/// [`MAX_COMPONENT_LEN`] bytes — which is the kernel's `FS_NAME_MAX`. This is
/// the one definition a tool that lets the user *name* something (the file
/// manager's rename and new-folder, a save-as field) judges a typed name
/// against before it reaches the VFS, so a name a tool accepts always spells
/// to the same single component and can never widen a path.
///
/// It is spelling only: like [`parse`], it opens nothing and performs no
/// capability check. A name that survives this may still be refused at the VFS
/// (an existing sibling, a read-only mount, a permission denial); that is the
/// kernel's decision, not this layer's.
///
/// # Errors
///
/// Returns the [`PathError`] naming the first rule the name breaks:
/// [`EmptyComponent`](PathError::EmptyComponent) for an empty name,
/// [`ReservedName`](PathError::ReservedName) for `.`/`..`,
/// [`SeparatorInName`](PathError::SeparatorInName) for a `/`,
/// [`ComponentTooLong`](PathError::ComponentTooLong) past
/// [`MAX_COMPONENT_LEN`], [`ControlCharacter`](PathError::ControlCharacter),
/// or [`ColonInComponent`](PathError::ColonInComponent).
pub fn validate_file_name(name: &str) -> Result<(), PathError> {
    if name.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if name == "." || name == ".." {
        return Err(PathError::ReservedName);
    }
    if name.contains('/') {
        return Err(PathError::SeparatorInName);
    }
    validate_component(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn components(p: &Path) -> Vec<String> {
        p.components().to_vec()
    }

    #[test]
    fn prefix_spells_each_truncation_and_rejects_overrun() {
        // A view path: prefixes run from the bare root to the full spelling.
        let p = parse("/a/b/c").expect("parse");
        assert_eq!(p.prefix(0).as_deref(), Some("/"));
        assert_eq!(p.prefix(1).as_deref(), Some("/a"));
        assert_eq!(p.prefix(2).as_deref(), Some("/a/b"));
        assert_eq!(p.prefix(3).as_deref(), Some("/a/b/c"));
        assert_eq!(p.prefix(4), None);
        // An alias path keeps its root spelling.
        let p = parse("Home:/tools/bin").expect("parse");
        assert_eq!(p.prefix(0).as_deref(), Some("Home:/"));
        assert_eq!(p.prefix(1).as_deref(), Some("Home:/tools"));
        assert_eq!(p.prefix(2).as_deref(), Some("Home:/tools/bin"));
        // A relative path's empty prefix is the current directory.
        let p = parse("x/y").expect("parse");
        assert_eq!(p.prefix(0).as_deref(), Some("."));
        assert_eq!(p.prefix(1).as_deref(), Some("x"));
        assert_eq!(p.prefix(2).as_deref(), Some("x/y"));
    }

    #[test]
    fn view_path() {
        let p = parse("/System/Kernel/tairix.rxe").unwrap();
        assert_eq!(p.root(), &Root::View);
        assert_eq!(components(&p), vec!["System", "Kernel", "tairix.rxe"]);
        assert!(p.is_absolute());
        assert_eq!(p.alias(), None);
        assert_eq!(p.to_string(), "/System/Kernel/tairix.rxe");
    }

    #[test]
    fn bare_view_root() {
        let p = parse("/").unwrap();
        assert_eq!(p.root(), &Root::View);
        assert!(p.components().is_empty());
        assert_eq!(p.to_string(), "/");
    }

    #[test]
    fn alias_shorthand() {
        let p = parse("Home:/Documents/spec.md").unwrap();
        assert_eq!(p.root(), &Root::Alias("Home".to_string()));
        assert_eq!(components(&p), vec!["Documents", "spec.md"]);
        assert_eq!(p.alias(), Some("Home"));
        assert_eq!(p.to_string(), "Home:/Documents/spec.md");
    }

    #[test]
    fn bare_alias_root() {
        let p = parse("Backup:/").unwrap();
        assert_eq!(p.root(), &Root::Alias("Backup".to_string()));
        assert!(p.components().is_empty());
        assert_eq!(p.to_string(), "Backup:/");
    }

    #[test]
    fn expanded_alias_equivalent_to_shorthand() {
        let a = parse("alias::Home/Documents/spec.md").unwrap();
        let b = parse("Home:/Documents/spec.md").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "Home:/Documents/spec.md");
    }

    #[test]
    fn expanded_alias_root_only() {
        let p = parse("alias::System").unwrap();
        assert_eq!(p.root(), &Root::Alias("System".to_string()));
        assert!(p.components().is_empty());
    }

    #[test]
    fn relative_path() {
        let p = parse("Documents/spec.md").unwrap();
        assert_eq!(p.root(), &Root::Relative);
        assert_eq!(components(&p), vec!["Documents", "spec.md"]);
        assert!(!p.is_absolute());
        assert_eq!(p.to_string(), "Documents/spec.md");
    }

    #[test]
    fn relative_dot_is_empty() {
        let p = parse(".").unwrap();
        assert_eq!(p.root(), &Root::Relative);
        assert!(p.components().is_empty());
        assert_eq!(p.to_string(), ".");
    }

    #[test]
    fn relative_leading_parent_preserved() {
        let p = parse("../../notes").unwrap();
        assert_eq!(p.root(), &Root::Relative);
        assert_eq!(components(&p), vec!["..", "..", "notes"]);
        assert_eq!(p.to_string(), "../../notes");
    }

    #[test]
    fn dot_and_dotdot_normalised() {
        let p = parse("Home:/a/./b/../c").unwrap();
        assert_eq!(components(&p), vec!["a", "c"]);
        assert_eq!(p.to_string(), "Home:/a/c");
    }

    #[test]
    fn interior_dotdot_in_relative() {
        let p = parse("a/b/../c").unwrap();
        assert_eq!(components(&p), vec!["a", "c"]);
    }

    #[test]
    fn dotdot_cannot_escape_alias_root() {
        assert_eq!(parse("Home:/.."), Err(PathError::EscapesRoot));
        assert_eq!(parse("Home:/a/../.."), Err(PathError::EscapesRoot));
    }

    #[test]
    fn dotdot_cannot_escape_view_root() {
        assert_eq!(parse("/.."), Err(PathError::EscapesRoot));
    }

    #[test]
    fn trailing_slash_ignored() {
        assert_eq!(parse("Home:/a/b/").unwrap().to_string(), "Home:/a/b");
        assert_eq!(parse("/a/").unwrap().to_string(), "/a");
    }

    #[test]
    fn empty_interior_component_rejected() {
        assert_eq!(parse("Home:/a//b"), Err(PathError::EmptyComponent));
        assert_eq!(parse("/a//b"), Err(PathError::EmptyComponent));
        assert_eq!(parse("a//b"), Err(PathError::EmptyComponent));
    }

    #[test]
    fn empty_input_rejected() {
        assert_eq!(parse(""), Err(PathError::Empty));
    }

    #[test]
    fn resource_reference_shape_is_not_a_path() {
        assert_eq!(parse("sys:random"), Err(PathError::NotAPath));
        assert_eq!(parse("disk:backup"), Err(PathError::NotAPath));
        // An alias path missing its `/` is the same shape and refused too.
        assert_eq!(parse("Home:Documents"), Err(PathError::NotAPath));
    }

    #[test]
    fn unsupported_resolvers_refused() {
        assert_eq!(
            parse("fs::arxfs/System/x"),
            Err(PathError::UnsupportedResolver)
        );
        assert_eq!(
            parse("arxfs::System/x"),
            Err(PathError::UnsupportedResolver)
        );
        assert_eq!(
            parse("dev::block/nvme0n1p2"),
            Err(PathError::UnsupportedResolver)
        );
        assert_eq!(
            parse("net::nas.local/x"),
            Err(PathError::UnsupportedResolver)
        );
    }

    const VOLUME_ID_TEXT: &str = "b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001";

    const VOLUME_ID_BYTES: [u8; 16] = [
        0xb7, 0xf2, 0xe4, 0xe6, 0x8d, 0x7a, 0x4e, 0xf8, 0xa1, 0x3e, 0xd3, 0xb8, 0x4d, 0x4e, 0x80,
        0x01,
    ];

    #[test]
    fn volume_id_path_parses_to_its_bytes() {
        let input = alloc::format!("id::{VOLUME_ID_TEXT}/Documents/file.txt");
        let p = parse(&input).unwrap();
        assert_eq!(
            p.root(),
            &Root::VolumeId(VolumeId::from_bytes(VOLUME_ID_BYTES))
        );
        assert_eq!(components(&p), vec!["Documents", "file.txt"]);
        assert!(p.is_absolute());
        assert_eq!(p.alias(), None);
        assert_eq!(p.volume_id(), Some(&VolumeId::from_bytes(VOLUME_ID_BYTES)));
        assert_eq!(p.to_string(), input);
    }

    #[test]
    fn bare_volume_id_root() {
        // Both the bare spelling and the trailing-slash rendering parse to
        // the same empty-component path.
        let bare = parse(&alloc::format!("id::{VOLUME_ID_TEXT}")).unwrap();
        let slashed = parse(&alloc::format!("id::{VOLUME_ID_TEXT}/")).unwrap();
        assert_eq!(bare, slashed);
        assert!(bare.components().is_empty());
        assert_eq!(bare.to_string(), alloc::format!("id::{VOLUME_ID_TEXT}/"));
        assert_eq!(
            bare.prefix(0).as_deref(),
            Some("id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/")
        );
    }

    #[test]
    fn volume_id_display_round_trips() {
        let input = alloc::format!("id::{VOLUME_ID_TEXT}/a/./b/../c");
        let p = parse(&input).unwrap();
        let rendered = p.to_string();
        assert_eq!(rendered, alloc::format!("id::{VOLUME_ID_TEXT}/a/c"));
        assert_eq!(parse(&rendered).unwrap(), p);
    }

    #[test]
    fn volume_id_cannot_escape_its_root() {
        assert_eq!(
            parse(&alloc::format!("id::{VOLUME_ID_TEXT}/..")),
            Err(PathError::EscapesRoot)
        );
    }

    #[test]
    fn non_canonical_volume_ids_rejected() {
        // Uppercase hex.
        assert_eq!(
            parse("id::B7F2E4E6-8D7A-4EF8-A13E-D3B84D4E8001/x"),
            Err(PathError::VolumeIdInvalid)
        );
        // Wrong length (truncated, extended, and empty).
        assert_eq!(
            parse("id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e800/x"),
            Err(PathError::VolumeIdInvalid)
        );
        assert_eq!(
            parse("id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e80011/x"),
            Err(PathError::VolumeIdInvalid)
        );
        assert_eq!(parse("id::/x"), Err(PathError::VolumeIdInvalid));
        assert_eq!(parse("id::"), Err(PathError::VolumeIdInvalid));
        // A misplaced hyphen and a non-hex digit.
        assert_eq!(
            parse("id::b7f2e4e68-d7a-4ef8-a13e-d3b84d4e8001/x"),
            Err(PathError::VolumeIdInvalid)
        );
        assert_eq!(
            parse("id::b7f2e4g6-8d7a-4ef8-a13e-d3b84d4e8001/x"),
            Err(PathError::VolumeIdInvalid)
        );
        // Braced/unhyphenated UUID spellings are not canonical.
        assert_eq!(
            parse("id::{b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001}/x"),
            Err(PathError::VolumeIdInvalid)
        );
        assert_eq!(
            parse("id::b7f2e4e68d7a4ef8a13ed3b84d4e8001/x"),
            Err(PathError::VolumeIdInvalid)
        );
    }

    #[test]
    fn volume_id_display_is_canonical() {
        let id = VolumeId::from_bytes(VOLUME_ID_BYTES);
        assert_eq!(id.to_string(), VOLUME_ID_TEXT);
        assert_eq!(*id.as_bytes(), VOLUME_ID_BYTES);
    }

    #[test]
    fn empty_resolver_token_rejected() {
        assert_eq!(parse("::foo"), Err(PathError::ResolverEmpty));
    }

    #[test]
    fn empty_alias_rejected() {
        assert_eq!(parse(":/foo"), Err(PathError::AliasEmpty));
        assert_eq!(parse("alias:://foo"), Err(PathError::AliasEmpty));
    }

    #[test]
    fn alias_invalid_char_rejected() {
        // A control character in the alias name.
        assert_eq!(parse("Ho\u{7}me:/x"), Err(PathError::AliasInvalidChar));
    }

    #[test]
    fn alias_too_long_rejected() {
        let name = "a".repeat(MAX_ALIAS_LEN + 1);
        let input = alloc::format!("{name}:/x");
        assert_eq!(parse(&input), Err(PathError::AliasTooLong));
    }

    #[test]
    fn colon_after_slash_is_not_a_root_delimiter_but_still_reserved() {
        // A `:` after a `/` is not a root delimiter (the path is relative), but
        // a colon is never allowed inside a component either, so it is refused
        // rather than producing a name that would not re-parse.
        assert_eq!(parse("dir/file:name"), Err(PathError::ColonInComponent));
    }

    #[test]
    fn too_long_input_rejected() {
        let input = "a".repeat(MAX_PATH_LEN + 1);
        assert_eq!(parse(&input), Err(PathError::TooLong));
    }

    #[test]
    fn component_too_long_rejected() {
        let long = "a".repeat(MAX_COMPONENT_LEN + 1);
        let input = alloc::format!("Home:/{long}");
        assert_eq!(parse(&input), Err(PathError::ComponentTooLong));
    }

    #[test]
    fn control_character_component_rejected() {
        assert_eq!(parse("Home:/a\u{0}b"), Err(PathError::ControlCharacter));
    }

    #[test]
    fn too_many_components_rejected() {
        let mut input = String::from("Home:/");
        for _ in 0..=MAX_COMPONENTS {
            input.push_str("a/");
        }
        assert_eq!(parse(&input), Err(PathError::TooManyComponents));
    }

    #[test]
    fn unicode_component_accepted() {
        let p = parse("Home:/café/photo").unwrap();
        assert_eq!(components(&p), vec!["café", "photo"]);
    }

    #[test]
    fn error_display_is_nonempty() {
        // Every variant renders a message (used in diagnostics).
        for e in [
            PathError::Empty,
            PathError::TooLong,
            PathError::TooManyComponents,
            PathError::ComponentTooLong,
            PathError::EmptyComponent,
            PathError::ControlCharacter,
            PathError::ColonInComponent,
            PathError::ReservedName,
            PathError::SeparatorInName,
            PathError::EscapesRoot,
            PathError::AliasEmpty,
            PathError::AliasTooLong,
            PathError::AliasInvalidChar,
            PathError::ResolverEmpty,
            PathError::VolumeIdInvalid,
            PathError::UnsupportedResolver,
            PathError::NotAPath,
        ] {
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn alias_root_len_reports_a_valid_alias_root_prefix() {
        assert_eq!(alias_root_len("Home:/"), Some(6));
        assert_eq!(alias_root_len("Home:/tools/x"), Some(6));
        // The remainder is untouched lexical text: a spelling `parse` would
        // reject still reports its prefix.
        assert_eq!(alias_root_len("Home:/a//b"), Some(6));
    }

    #[test]
    fn validate_file_name_accepts_an_ordinary_name() {
        assert_eq!(validate_file_name("report.md"), Ok(()));
        assert_eq!(validate_file_name("café"), Ok(()));
        assert_eq!(validate_file_name("Example.app"), Ok(()));
        // A leading dot is a hidden file, not the `.` navigation name.
        assert_eq!(validate_file_name(".config"), Ok(()));
        // The full-length boundary is accepted.
        assert_eq!(validate_file_name(&"a".repeat(MAX_COMPONENT_LEN)), Ok(()));
    }

    #[test]
    fn validate_file_name_rejects_each_broken_rule() {
        assert_eq!(validate_file_name(""), Err(PathError::EmptyComponent));
        assert_eq!(validate_file_name("."), Err(PathError::ReservedName));
        assert_eq!(validate_file_name(".."), Err(PathError::ReservedName));
        assert_eq!(validate_file_name("a/b"), Err(PathError::SeparatorInName));
        assert_eq!(validate_file_name("/"), Err(PathError::SeparatorInName));
        assert_eq!(
            validate_file_name("a\u{0}b"),
            Err(PathError::ControlCharacter)
        );
        assert_eq!(
            validate_file_name("Backup:x"),
            Err(PathError::ColonInComponent)
        );
        assert_eq!(
            validate_file_name(&"a".repeat(MAX_COMPONENT_LEN + 1)),
            Err(PathError::ComponentTooLong)
        );
    }

    #[test]
    fn alias_root_len_refuses_non_alias_roots() {
        // View, relative, and empty spellings carry no alias root.
        assert_eq!(alias_root_len("/System/Apps"), None);
        assert_eq!(alias_root_len("notes/todo"), None);
        assert_eq!(alias_root_len(""), None);
        // A resolver spelling and a resource reference are not alias roots.
        assert_eq!(alias_root_len("alias::Home/x"), None);
        assert_eq!(alias_root_len("sys:random"), None);
        // A `:` after the first `/` is an ordinary component character.
        assert_eq!(alias_root_len("a/b:/c"), None);
        // An invalid alias name is not a root.
        assert_eq!(alias_root_len(":/x"), None);
        assert_eq!(alias_root_len("Ho/me:/x"), None);
    }
}
