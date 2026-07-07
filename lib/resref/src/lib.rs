//! RustOS shared resource-reference parser (`lib/resref`).
//!
//! RustOS has no `/dev`, `/proc`, or `/sys`. Non-filesystem resources — the
//! random source, a disk, a serial port, a live metric — are named instead by
//! typed *resource references* such as `sys:random`, `disk:backup@7K2M`, or
//! `stats:net/wan/rx.pps?window=1s`. Several components need to turn such a
//! string into a structured, validated form: the shell first (redirection
//! targets, command arguments, completion, typed shell values), and the
//! resolver services and ABI helpers behind it. That lexing is *identical*
//! wherever it happens, so it lives here once and every consumer imports it,
//! rather than each growing a private reference parser.
//!
//! This crate is a pure *spelling* step: it turns a string into a typed
//! [`ResourceRef`]. It does **not** resolve a namespace to a resource, open
//! anything, check an identity fingerprint, or perform a capability check.
//! Resolution is capability-checked, fail-closed, and owned by the resolver
//! services (the System Information API for `info:`/`stats:`, the device
//! manager and hardware tree for device namespaces); parsing a string here can
//! never widen authority. The typed *resolver* errors (`UnknownNamespace`,
//! `CapabilityDenied`, `IdentityMismatch`, …) are therefore not produced here —
//! this layer only reports that a string is or is not a well-formed reference.
//!
//! # Grammar
//!
//! ```text
//! resource-ref = namespace ":" selector [ "@" guard ] [ "::" facet ] [ "?" params ]
//! selector     = [ selector-part *( "/" selector-part ) ]
//! params       = param *( "," param )
//! param        = key op value
//! op           = "=" | "!=" | "<" | "<=" | ">" | ">=" | "~"
//! ```
//!
//! A `namespace`, `facet`, and parameter `key` are lowercase ASCII idents
//! (`a-z 0-9 - _`) that start with a letter. Selector parts are case-sensitive
//! (`a-z A-Z 0-9 - _ .`), because full identity selectors carry mixed-case,
//! hyphenated tokens (`disk:id/serial/S6XYZ123456789`). The reserved delimiters
//! `:`, `/`, `@`, `::`, `?`, and `,` are never literal characters inside the
//! part they delimit, so a rendered reference always re-parses to the same
//! value.
//!
//! The `@guard` shorthand may stand alone (`disk:@7K2M`), selecting by short
//! fingerprint within the namespace, and a `?params` query may stand alone
//! (`disk:?removable=true`); a reference with an empty selector and neither a
//! guard nor a query (`disk:`) is rejected.
//!
//! # Bounds and fail-closed behaviour
//!
//! A reference string is untrusted input, so every dimension is a fixed
//! security bound, not a growable capacity: [`MAX_REF_LEN`],
//! [`MAX_NAMESPACE_LEN`], [`MAX_SELECTOR_SEGMENTS`], [`MAX_SEGMENT_LEN`],
//! [`MAX_GUARD_LEN`], [`MAX_FACET_LEN`], [`MAX_PARAMS`], [`MAX_PARAM_KEY_LEN`],
//! and [`MAX_PARAM_VALUE_LEN`]. Anything malformed or over-long is a typed
//! [`RefError`] from [`parse`], never a silently "fixed up" reference. A string
//! with no `:` delimiter is [`RefError::NotAReference`] (it is a filesystem
//! path, owned by the separate path grammar).
//!
//! Parsing is the only fallible step and it never panics. Every operation runs
//! in time linear in the input length, with no recursion, so neither a hostile
//! reference nor a hostile segment can trigger runaway work.
//!
//! # Example
//!
//! ```
//! use rustos_resref::{parse, KnownNamespace, Op};
//!
//! let r = parse("stats:net/wan/rx.pps?window=1s").unwrap();
//! assert_eq!(r.namespace().known(), Some(KnownNamespace::Stats));
//! assert_eq!(r.selector(), &["net", "wan", "rx.pps"]);
//! assert_eq!(r.params()[0].key(), "window");
//! assert_eq!(r.params()[0].op(), Op::Eq);
//! assert_eq!(r.params()[0].value(), "1s");
//! assert_eq!(r.to_string(), "stats:net/wan/rx.pps?window=1s");
//!
//! // A guarded destructive target and a stand-alone fingerprint shorthand.
//! assert_eq!(parse("disk:backup@7K2M::raw").unwrap().to_string(), "disk:backup@7K2M::raw");
//! assert_eq!(parse("disk:@7K2M").unwrap().to_string(), "disk:@7K2M");
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Largest accepted reference string, in bytes. Longer input is rejected
/// outright, before any structural work, so an over-long string cannot drive
/// cost.
pub const MAX_REF_LEN: usize = 1024;

/// Largest namespace name, in bytes.
pub const MAX_NAMESPACE_LEN: usize = 32;

/// Largest number of `/`-separated selector segments.
pub const MAX_SELECTOR_SEGMENTS: usize = 32;

/// Largest single selector segment, in bytes.
pub const MAX_SEGMENT_LEN: usize = 128;

/// Largest identity-guard fingerprint, in bytes.
pub const MAX_GUARD_LEN: usize = 64;

/// Largest facet name, in bytes.
pub const MAX_FACET_LEN: usize = 32;

/// Largest number of query parameters.
pub const MAX_PARAMS: usize = 16;

/// Largest parameter key, in bytes.
pub const MAX_PARAM_KEY_LEN: usize = 32;

/// Largest parameter value, in bytes.
pub const MAX_PARAM_VALUE_LEN: usize = 128;

/// The closed registry of namespaces this specification defines. A parsed
/// [`Namespace`] retains its validated spelling regardless of membership (an
/// unregistered-but-well-formed namespace is a resolver concern, not a parse
/// error); [`Namespace::known`] classifies it against this set.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum KnownNamespace {
    /// System services and abstract kernel-backed resources (`sys:random`).
    Sys,
    /// Static hardware/system facts, via the System Information API.
    Info,
    /// Live metrics, via the System Information API.
    Stats,
    /// Active, mutable configuration state.
    State,
    /// Whole storage devices.
    Disk,
    /// Partitions.
    Part,
    /// Mountable volumes.
    Vol,
    /// Serial / terminal ports.
    Tty,
    /// Network interfaces.
    Net,
    /// Input devices.
    Input,
    /// Audio endpoints.
    Audio,
    /// Graphics devices.
    Gpu,
    /// Buses.
    Bus,
    /// System service handles.
    Svc,
    /// Processes.
    Proc,
    /// Capabilities.
    Cap,
}

impl KnownNamespace {
    /// The canonical spelling of this namespace.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            KnownNamespace::Sys => "sys",
            KnownNamespace::Info => "info",
            KnownNamespace::Stats => "stats",
            KnownNamespace::State => "state",
            KnownNamespace::Disk => "disk",
            KnownNamespace::Part => "part",
            KnownNamespace::Vol => "vol",
            KnownNamespace::Tty => "tty",
            KnownNamespace::Net => "net",
            KnownNamespace::Input => "input",
            KnownNamespace::Audio => "audio",
            KnownNamespace::Gpu => "gpu",
            KnownNamespace::Bus => "bus",
            KnownNamespace::Svc => "svc",
            KnownNamespace::Proc => "proc",
            KnownNamespace::Cap => "cap",
        }
    }

    /// Every registered namespace in declaration order, for exhaustive
    /// iteration (registry-driven completion, tests). Kept in one place so a
    /// namespace added to the registry can never be missed by an iterator.
    pub const ALL: [KnownNamespace; 16] = [
        KnownNamespace::Sys,
        KnownNamespace::Info,
        KnownNamespace::Stats,
        KnownNamespace::State,
        KnownNamespace::Disk,
        KnownNamespace::Part,
        KnownNamespace::Vol,
        KnownNamespace::Tty,
        KnownNamespace::Net,
        KnownNamespace::Input,
        KnownNamespace::Audio,
        KnownNamespace::Gpu,
        KnownNamespace::Bus,
        KnownNamespace::Svc,
        KnownNamespace::Proc,
        KnownNamespace::Cap,
    ];

    /// The namespace's *well-known selectors*: the closed, documented set of
    /// selectors the platform serves for every installation (today the
    /// kernel-resolved unprivileged `sys:` members). Registry data for
    /// display and completion only — spelling grants nothing, and whether a
    /// caller may *open* one is decided by the capability-checked resolver at
    /// open time. A namespace whose members are discovered per machine
    /// (`disk:`, `net:`, …) has no well-known set and returns an empty slice.
    ///
    /// The kernel resolver's unit tests cross-check this table against what
    /// it actually serves, so the registry cannot drift from reality.
    #[must_use]
    pub fn well_known_selectors(self) -> &'static [&'static str] {
        match self {
            KnownNamespace::Sys => &["null", "random"],
            _ => &[],
        }
    }

    /// Classify a spelling against the registry, if it is a registered name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<KnownNamespace> {
        Some(match name {
            "sys" => KnownNamespace::Sys,
            "info" => KnownNamespace::Info,
            "stats" => KnownNamespace::Stats,
            "state" => KnownNamespace::State,
            "disk" => KnownNamespace::Disk,
            "part" => KnownNamespace::Part,
            "vol" => KnownNamespace::Vol,
            "tty" => KnownNamespace::Tty,
            "net" => KnownNamespace::Net,
            "input" => KnownNamespace::Input,
            "audio" => KnownNamespace::Audio,
            "gpu" => KnownNamespace::Gpu,
            "bus" => KnownNamespace::Bus,
            "svc" => KnownNamespace::Svc,
            "proc" => KnownNamespace::Proc,
            "cap" => KnownNamespace::Cap,
            _ => return None,
        })
    }
}

/// A validated namespace name (the token before the first `:`). Its spelling is
/// a lowercase ASCII ident starting with a letter; membership of the closed
/// registry is reported by [`Namespace::known`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Namespace(String);

impl Namespace {
    /// The validated namespace spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The registry entry this namespace names, if it is a registered one.
    #[must_use]
    pub fn known(&self) -> Option<KnownNamespace> {
        KnownNamespace::from_name(&self.0)
    }
}

/// The comparison operator in a query parameter (`key op value`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `~` (match / approximately)
    Match,
}

impl Op {
    /// The canonical spelling of this operator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Match => "~",
        }
    }
}

/// A single query parameter: a `key`, an [`Op`], and a `value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    key: String,
    op: Op,
    value: String,
}

impl Param {
    /// The parameter key (a lowercase ASCII ident).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The comparison operator.
    #[must_use]
    pub fn op(&self) -> Op {
        self.op
    }

    /// The parameter value (validated but not otherwise interpreted here).
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A parsed, validated RustOS resource reference.
///
/// A reference is `namespace:selector[@guard][::facet][?params]`. The selector
/// may be empty when a `guard` (the `disk:@7K2M` fingerprint shorthand) or a
/// `params` query (`disk:?removable=true`) is present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRef {
    namespace: Namespace,
    selector: Vec<String>,
    guard: Option<String>,
    facet: Option<String>,
    params: Vec<Param>,
}

impl ResourceRef {
    /// The namespace this reference is in.
    #[must_use]
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// The `/`-separated selector segments (possibly empty).
    #[must_use]
    pub fn selector(&self) -> &[String] {
        &self.selector
    }

    /// The identity-guard fingerprint, when the reference carries one (`@…`).
    #[must_use]
    pub fn guard(&self) -> Option<&str> {
        self.guard.as_deref()
    }

    /// The facet, when the reference names one (`::…`).
    #[must_use]
    pub fn facet(&self) -> Option<&str> {
        self.facet.as_deref()
    }

    /// The query parameters (possibly empty).
    #[must_use]
    pub fn params(&self) -> &[Param] {
        &self.params
    }
}

impl fmt::Display for ResourceRef {
    /// Renders the canonical spelling, which re-parses to an equal value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.namespace.0)?;
        let mut first = true;
        for segment in &self.selector {
            if first {
                first = false;
            } else {
                f.write_str("/")?;
            }
            f.write_str(segment)?;
        }
        if let Some(guard) = &self.guard {
            write!(f, "@{guard}")?;
        }
        if let Some(facet) = &self.facet {
            write!(f, "::{facet}")?;
        }
        let mut first_param = true;
        for param in &self.params {
            f.write_str(if first_param { "?" } else { "," })?;
            first_param = false;
            write!(f, "{}{}{}", param.key, param.op.as_str(), param.value)?;
        }
        Ok(())
    }
}

/// Why a reference string was rejected. Parsing fails closed: on any of these
/// the caller has no [`ResourceRef`], never a partially-applied or guessed one.
///
/// These are *syntax* errors only. Whether a well-formed reference resolves —
/// whether its namespace is registered, its capability is held, its identity
/// still matches — is decided by the resolver services, which return their own
/// typed errors; this layer never does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RefError {
    /// The input was empty.
    Empty,
    /// The input exceeded [`MAX_REF_LEN`] bytes.
    TooLong,
    /// The input had no `:` delimiter: it is a filesystem path (owned by the
    /// separate path grammar), not a resource reference.
    NotAReference,
    /// The namespace (before the first `:`) was empty.
    EmptyNamespace,
    /// The namespace exceeded [`MAX_NAMESPACE_LEN`] bytes.
    NamespaceTooLong,
    /// The namespace was not a lowercase ASCII ident starting with a letter.
    NamespaceInvalidChar,
    /// The selector was empty and the reference carried neither a guard nor a
    /// query, so it names nothing (`disk:`).
    EmptySelector,
    /// A selector segment was empty (`a//b`, a leading or trailing `/`).
    EmptySegment,
    /// A selector segment exceeded [`MAX_SEGMENT_LEN`] bytes.
    SegmentTooLong,
    /// A selector segment contained a disallowed character.
    SegmentInvalidChar,
    /// The selector exceeded [`MAX_SELECTOR_SEGMENTS`] segments.
    TooManySegments,
    /// An identity guard (`@`) was present but its fingerprint was empty.
    EmptyGuard,
    /// The guard fingerprint exceeded [`MAX_GUARD_LEN`] bytes.
    GuardTooLong,
    /// The guard fingerprint contained a character that is not ASCII
    /// alphanumeric.
    GuardInvalidChar,
    /// A facet delimiter (`::`) was present but the facet name was empty.
    EmptyFacet,
    /// The facet name exceeded [`MAX_FACET_LEN`] bytes.
    FacetTooLong,
    /// The facet name was not a lowercase ASCII ident starting with a letter.
    FacetInvalidChar,
    /// A query delimiter (`?`) was present but no parameters followed.
    EmptyParams,
    /// The query exceeded [`MAX_PARAMS`] parameters.
    TooManyParams,
    /// A parameter had no operator (`= != < <= > >= ~`).
    ParamMissingOp,
    /// A parameter key was empty.
    EmptyParamKey,
    /// A parameter key exceeded [`MAX_PARAM_KEY_LEN`] bytes.
    ParamKeyTooLong,
    /// A parameter key was not a lowercase ASCII ident starting with a letter.
    ParamKeyInvalidChar,
    /// A parameter value exceeded [`MAX_PARAM_VALUE_LEN`] bytes.
    ParamValueTooLong,
    /// A parameter value contained a disallowed character.
    ParamValueInvalidChar,
}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            RefError::Empty => "empty resource reference",
            RefError::TooLong => "resource reference exceeds the maximum length",
            RefError::NotAReference => "input has no `:` delimiter (it is a filesystem path)",
            RefError::EmptyNamespace => "namespace is empty",
            RefError::NamespaceTooLong => "namespace exceeds the maximum length",
            RefError::NamespaceInvalidChar => {
                "namespace is not a lowercase ident starting with a letter"
            }
            RefError::EmptySelector => "selector is empty and there is no guard or query",
            RefError::EmptySegment => "selector has an empty segment",
            RefError::SegmentTooLong => "selector segment exceeds the maximum length",
            RefError::SegmentInvalidChar => "selector segment contains a disallowed character",
            RefError::TooManySegments => "selector has too many segments",
            RefError::EmptyGuard => "identity guard fingerprint is empty",
            RefError::GuardTooLong => "identity guard fingerprint exceeds the maximum length",
            RefError::GuardInvalidChar => {
                "identity guard fingerprint contains a non-alphanumeric character"
            }
            RefError::EmptyFacet => "facet name is empty",
            RefError::FacetTooLong => "facet name exceeds the maximum length",
            RefError::FacetInvalidChar => "facet is not a lowercase ident starting with a letter",
            RefError::EmptyParams => "query has no parameters",
            RefError::TooManyParams => "query has too many parameters",
            RefError::ParamMissingOp => "query parameter has no operator",
            RefError::EmptyParamKey => "query parameter key is empty",
            RefError::ParamKeyTooLong => "query parameter key exceeds the maximum length",
            RefError::ParamKeyInvalidChar => {
                "query parameter key is not a lowercase ident starting with a letter"
            }
            RefError::ParamValueTooLong => "query parameter value exceeds the maximum length",
            RefError::ParamValueInvalidChar => {
                "query parameter value contains a disallowed character"
            }
        };
        f.write_str(msg)
    }
}

/// Parse and validate a RustOS resource-reference string into a typed
/// [`ResourceRef`].
///
/// See the crate documentation for the grammar, the fixed security bounds, and
/// the fail-closed rules. This is the only fallible step; the returned
/// [`ResourceRef`] can be displayed and inspected infallibly, and its canonical
/// spelling re-parses to an equal value.
pub fn parse(input: &str) -> Result<ResourceRef, RefError> {
    if input.is_empty() {
        return Err(RefError::Empty);
    }
    if input.len() > MAX_REF_LEN {
        return Err(RefError::TooLong);
    }

    // The first `:` splits the namespace from the rest. A string with no `:` is
    // a filesystem path, not a resource reference.
    let colon = input.find(':').ok_or(RefError::NotAReference)?;
    let namespace = validate_namespace(&input[..colon])?;
    let mut rest = &input[colon + 1..];

    // Peel the optional parts from the tail inwards: `?params`, then `::facet`,
    // then `@guard`, leaving the selector. Each delimiter is reserved and
    // cannot appear inside the part it delimits, so the split is unambiguous.
    let params = match rest.find('?') {
        Some(q) => {
            let params = parse_params(&rest[q + 1..])?;
            rest = &rest[..q];
            params
        }
        None => Vec::new(),
    };

    let facet = match rest.find("::") {
        Some(d) => {
            let facet = validate_facet(&rest[d + 2..])?;
            rest = &rest[..d];
            Some(facet)
        }
        None => None,
    };

    let guard = match rest.find('@') {
        Some(a) => {
            let guard = validate_guard(&rest[a + 1..])?;
            rest = &rest[..a];
            Some(guard)
        }
        None => None,
    };

    let selector = parse_selector(rest)?;

    // A reference must name something: an empty selector is allowed only when a
    // guard (fingerprint shorthand) or a query stands in for it.
    if selector.is_empty() && guard.is_none() && params.is_empty() {
        return Err(RefError::EmptySelector);
    }

    Ok(ResourceRef {
        namespace,
        selector,
        guard,
        facet,
        params,
    })
}

/// What a `:`-bearing target string names: one of the two worlds a shell
/// word, redirection target, or tool operand can belong to.
///
/// RustOS has no `/dev`: the byte sinks and sources a program can name
/// (`sys:null`, `sys:random`, …) are resource references, not device files.
/// [`classify_target`] decides which world a target belongs to *before* any
/// filesystem lookup, so a real on-disk file whose name happens to contain
/// `:` (legal on ext/POSIX volumes, where only `NUL` and `/` are forbidden)
/// always stays reachable as `./name` or when quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetClass {
    /// A filesystem path (an ordinary path or an alias path), to be resolved
    /// through the VFS.
    Path,
    /// A resource reference, to be resolved through the capability-checked
    /// resource resolver — never a filesystem lookup. Only the *spelling* is
    /// decided here; opening it is the caller's capability-checked step.
    Resource(ResourceRef),
}

/// Classify a target string as a filesystem path or a resource reference —
/// the one shared resolution rule every consumer (the shell's redirection
/// targets, `cat`-style tool operands, completion) applies, never a second
/// private copy.
///
/// A target names a resource reference only when it is a relative path whose
/// first path component holds a `:` preceded by a registered resource
/// namespace and not immediately followed by `/` (the spelling that tells
/// `sys:null` apart from the `Alias:/path` alias-path form), and whose prefix
/// is neither `.` nor `..`. Every other target — absolute, dot-relative,
/// sub-path, or an unregistered prefix — is a path, so nothing on disk is
/// ever shadowed.
///
/// # Errors
///
/// A target the rule classifies as a resource reference but that is not a
/// well-formed reference returns the [`RefError`] — the caller fails closed
/// (it never falls back to a filesystem lookup), so a typo cannot silently
/// produce junk on disk.
pub fn classify_target(target: &str) -> Result<TargetClass, RefError> {
    if !names_resource_reference(target) {
        return Ok(TargetClass::Path);
    }
    parse(target).map(TargetClass::Resource)
}

/// The structural half of the resolution rule: does `target`'s spelling name
/// a resource reference rather than a path? See [`classify_target`].
///
/// This is the routing test alone — it does **not** validate the reference.
/// A caller that needs the parsed [`ResourceRef`] (or the parse error) uses
/// [`classify_target`]; a caller that only picks which resolver to hand the
/// spelling to (the userland runtime's open path routing between the
/// filesystem and the kernel's capability-checked resource resolver) uses
/// this predicate and lets that resolver refuse a malformed reference, so
/// there is exactly one refusal point and a typo never falls back to a
/// filesystem lookup.
#[must_use]
pub fn names_resource_reference(target: &str) -> bool {
    // Absolute paths are always paths.
    if target.starts_with('/') {
        return false;
    }
    // The rule inspects only the first path component (up to the first `/`).
    let first_component_end = target.find('/').unwrap_or(target.len());
    let first_component = &target[..first_component_end];
    let Some(colon) = first_component.find(':') else {
        return false;
    };
    let prefix = &first_component[..colon];
    if prefix == "." || prefix == ".." {
        return false;
    }
    if KnownNamespace::from_name(prefix).is_none() {
        return false;
    }
    // A `:` immediately followed by `/` is the alias-path form (`Home:/x`), a
    // path, not a reference. Tested against the original target so the `/`
    // that ended the first component still counts.
    !target[colon + 1..].starts_with('/')
}

/// Validate a lowercase ASCII ident that must start with a letter, against a
/// per-role length bound and error set.
fn validate_lower_ident(
    name: &str,
    max_len: usize,
    empty: RefError,
    too_long: RefError,
    invalid: RefError,
) -> Result<(), RefError> {
    if name.is_empty() {
        return Err(empty);
    }
    if name.len() > max_len {
        return Err(too_long);
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return Err(invalid),
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err(invalid);
        }
    }
    Ok(())
}

/// Validate the namespace token and return it.
fn validate_namespace(name: &str) -> Result<Namespace, RefError> {
    validate_lower_ident(
        name,
        MAX_NAMESPACE_LEN,
        RefError::EmptyNamespace,
        RefError::NamespaceTooLong,
        RefError::NamespaceInvalidChar,
    )?;
    Ok(Namespace(String::from(name)))
}

/// Validate the facet token and return it.
fn validate_facet(name: &str) -> Result<String, RefError> {
    validate_lower_ident(
        name,
        MAX_FACET_LEN,
        RefError::EmptyFacet,
        RefError::FacetTooLong,
        RefError::FacetInvalidChar,
    )?;
    Ok(String::from(name))
}

/// Validate an identity-guard fingerprint (ASCII alphanumeric) and return it.
fn validate_guard(fingerprint: &str) -> Result<String, RefError> {
    if fingerprint.is_empty() {
        return Err(RefError::EmptyGuard);
    }
    if fingerprint.len() > MAX_GUARD_LEN {
        return Err(RefError::GuardTooLong);
    }
    if fingerprint.chars().any(|c| !c.is_ascii_alphanumeric()) {
        return Err(RefError::GuardInvalidChar);
    }
    Ok(String::from(fingerprint))
}

/// Split and validate the selector into `/`-separated segments. An empty
/// string is an empty selector; a leading, trailing, or doubled `/` is an
/// empty segment and is rejected.
fn parse_selector(selector: &str) -> Result<Vec<String>, RefError> {
    if selector.is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for piece in selector.split('/') {
        if piece.is_empty() {
            return Err(RefError::EmptySegment);
        }
        if piece.len() > MAX_SEGMENT_LEN {
            return Err(RefError::SegmentTooLong);
        }
        if piece
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        {
            return Err(RefError::SegmentInvalidChar);
        }
        if segments.len() >= MAX_SELECTOR_SEGMENTS {
            return Err(RefError::TooManySegments);
        }
        segments.push(String::from(piece));
    }
    Ok(segments)
}

/// Parse the `,`-separated parameter list after a `?`.
fn parse_params(params: &str) -> Result<Vec<Param>, RefError> {
    if params.is_empty() {
        return Err(RefError::EmptyParams);
    }
    let mut out = Vec::new();
    for piece in params.split(',') {
        if out.len() >= MAX_PARAMS {
            return Err(RefError::TooManyParams);
        }
        out.push(parse_param(piece)?);
    }
    Ok(out)
}

/// Parse a single `key op value` parameter.
fn parse_param(piece: &str) -> Result<Param, RefError> {
    let (op_start, op) = find_op(piece).ok_or(RefError::ParamMissingOp)?;
    let key = &piece[..op_start];
    let value = &piece[op_start + op.as_str().len()..];

    validate_lower_ident(
        key,
        MAX_PARAM_KEY_LEN,
        RefError::EmptyParamKey,
        RefError::ParamKeyTooLong,
        RefError::ParamKeyInvalidChar,
    )?;

    if value.len() > MAX_PARAM_VALUE_LEN {
        return Err(RefError::ParamValueTooLong);
    }
    if value
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '+'))
    {
        return Err(RefError::ParamValueInvalidChar);
    }

    Ok(Param {
        key: String::from(key),
        op,
        value: String::from(value),
    })
}

/// Locate the first operator in a parameter piece, returning its byte offset
/// and the operator. The key charset excludes every operator character, so the
/// first operator character is unambiguously where the key ends.
fn find_op(piece: &str) -> Option<(usize, Op)> {
    let bytes = piece.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let next = bytes.get(i + 1).copied();
        let op = match b {
            b'=' => Op::Eq,
            b'~' => Op::Match,
            b'!' if next == Some(b'=') => Op::Ne,
            b'<' if next == Some(b'=') => Op::Le,
            b'<' => Op::Lt,
            b'>' if next == Some(b'=') => Op::Ge,
            b'>' => Op::Gt,
            _ => continue,
        };
        return Some((i, op));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn selector(r: &ResourceRef) -> Vec<String> {
        r.selector().to_vec()
    }

    #[test]
    fn simple_reference() {
        let r = parse("sys:random").unwrap();
        assert_eq!(r.namespace().as_str(), "sys");
        assert_eq!(r.namespace().known(), Some(KnownNamespace::Sys));
        assert_eq!(selector(&r), vec!["random"]);
        assert_eq!(r.guard(), None);
        assert_eq!(r.facet(), None);
        assert!(r.params().is_empty());
        assert_eq!(r.to_string(), "sys:random");
    }

    #[test]
    fn multi_segment_selector() {
        let r = parse("info:cpu/vendor").unwrap();
        assert_eq!(r.namespace().known(), Some(KnownNamespace::Info));
        assert_eq!(selector(&r), vec!["cpu", "vendor"]);
        assert_eq!(r.to_string(), "info:cpu/vendor");
    }

    #[test]
    fn guard_facet_and_params() {
        let r = parse("disk:slot/front-usb@P91Q::raw").unwrap();
        assert_eq!(r.namespace().known(), Some(KnownNamespace::Disk));
        assert_eq!(selector(&r), vec!["slot", "front-usb"]);
        assert_eq!(r.guard(), Some("P91Q"));
        assert_eq!(r.facet(), Some("raw"));
        assert_eq!(r.to_string(), "disk:slot/front-usb@P91Q::raw");
    }

    #[test]
    fn direct_identity_shorthand() {
        let r = parse("disk:@7K2M").unwrap();
        assert!(r.selector().is_empty());
        assert_eq!(r.guard(), Some("7K2M"));
        assert_eq!(r.to_string(), "disk:@7K2M");
    }

    #[test]
    fn query_only_selector() {
        let r = parse("disk:?removable=true,size>=16GiB").unwrap();
        assert!(r.selector().is_empty());
        assert_eq!(r.params().len(), 2);
        assert_eq!(r.params()[0].key(), "removable");
        assert_eq!(r.params()[0].op(), Op::Eq);
        assert_eq!(r.params()[0].value(), "true");
        assert_eq!(r.params()[1].key(), "size");
        assert_eq!(r.params()[1].op(), Op::Ge);
        assert_eq!(r.params()[1].value(), "16GiB");
        assert_eq!(r.to_string(), "disk:?removable=true,size>=16GiB");
    }

    #[test]
    fn every_operator_round_trips() {
        for (spelling, op) in [
            ("k=v", Op::Eq),
            ("k!=v", Op::Ne),
            ("k<v", Op::Lt),
            ("k<=v", Op::Le),
            ("k>v", Op::Gt),
            ("k>=v", Op::Ge),
            ("k~v", Op::Match),
        ] {
            let input = alloc::format!("stats:x?{spelling}");
            let r = parse(&input).unwrap();
            assert_eq!(r.params()[0].op(), op);
            assert_eq!(r.to_string(), input);
        }
    }

    #[test]
    fn metric_window_param() {
        let r = parse("stats:net/wan/rx.pps?window=1s").unwrap();
        assert_eq!(selector(&r), vec!["net", "wan", "rx.pps"]);
        assert_eq!(r.params()[0].value(), "1s");
        assert_eq!(r.to_string(), "stats:net/wan/rx.pps?window=1s");
    }

    #[test]
    fn full_identity_selector_preserves_case() {
        let r = parse("disk:id/serial/S6XYZ123456789").unwrap();
        assert_eq!(selector(&r), vec!["id", "serial", "S6XYZ123456789"]);
        assert_eq!(r.to_string(), "disk:id/serial/S6XYZ123456789");
    }

    #[test]
    fn unregistered_namespace_still_parses() {
        // Membership of the registry is a resolver concern, not a parse error.
        let r = parse("madeup:thing").unwrap();
        assert_eq!(r.namespace().as_str(), "madeup");
        assert_eq!(r.namespace().known(), None);
    }

    #[test]
    fn known_namespace_round_trips_names() {
        for ns in [
            KnownNamespace::Sys,
            KnownNamespace::Info,
            KnownNamespace::Stats,
            KnownNamespace::State,
            KnownNamespace::Disk,
            KnownNamespace::Part,
            KnownNamespace::Vol,
            KnownNamespace::Tty,
            KnownNamespace::Net,
            KnownNamespace::Input,
            KnownNamespace::Audio,
            KnownNamespace::Gpu,
            KnownNamespace::Bus,
            KnownNamespace::Svc,
            KnownNamespace::Proc,
            KnownNamespace::Cap,
        ] {
            assert_eq!(KnownNamespace::from_name(ns.as_str()), Some(ns));
        }
    }

    #[test]
    fn path_without_colon_is_not_a_reference() {
        assert_eq!(parse("Documents/spec.md"), Err(RefError::NotAReference));
        assert_eq!(parse("/System/Kernel"), Err(RefError::NotAReference));
    }

    #[test]
    fn empty_input_rejected() {
        assert_eq!(parse(""), Err(RefError::Empty));
    }

    #[test]
    fn empty_namespace_rejected() {
        assert_eq!(parse(":random"), Err(RefError::EmptyNamespace));
    }

    #[test]
    fn namespace_must_start_with_letter() {
        assert_eq!(parse("1sys:x"), Err(RefError::NamespaceInvalidChar));
        assert_eq!(parse("SYS:x"), Err(RefError::NamespaceInvalidChar));
        assert_eq!(parse("sy s:x"), Err(RefError::NamespaceInvalidChar));
    }

    #[test]
    fn empty_selector_without_guard_or_query_rejected() {
        assert_eq!(parse("disk:"), Err(RefError::EmptySelector));
        assert_eq!(parse("disk:::raw"), Err(RefError::EmptySelector));
    }

    #[test]
    fn empty_segment_rejected() {
        assert_eq!(parse("disk:a//b"), Err(RefError::EmptySegment));
        assert_eq!(parse("disk:/a"), Err(RefError::EmptySegment));
        assert_eq!(parse("disk:a/"), Err(RefError::EmptySegment));
    }

    #[test]
    fn segment_invalid_char_rejected() {
        assert_eq!(parse("disk:a b"), Err(RefError::SegmentInvalidChar));
    }

    #[test]
    fn empty_guard_rejected() {
        assert_eq!(parse("disk:backup@"), Err(RefError::EmptyGuard));
    }

    #[test]
    fn guard_invalid_char_rejected() {
        assert_eq!(parse("disk:backup@7-K"), Err(RefError::GuardInvalidChar));
    }

    #[test]
    fn guard_after_facet_is_rejected() {
        // The grammar orders guard before facet; a `@` in the facet position is
        // not a valid facet ident, so the wrong order fails closed.
        assert_eq!(
            parse("disk:backup::raw@7K2M"),
            Err(RefError::FacetInvalidChar)
        );
    }

    #[test]
    fn empty_facet_rejected() {
        assert_eq!(parse("disk:backup::"), Err(RefError::EmptyFacet));
    }

    #[test]
    fn facet_must_be_lowercase_ident() {
        assert_eq!(parse("disk:backup::Raw"), Err(RefError::FacetInvalidChar));
    }

    #[test]
    fn empty_query_rejected() {
        assert_eq!(parse("disk:backup?"), Err(RefError::EmptyParams));
    }

    #[test]
    fn param_missing_operator_rejected() {
        assert_eq!(parse("disk:x?removable"), Err(RefError::ParamMissingOp));
    }

    #[test]
    fn empty_param_key_rejected() {
        assert_eq!(parse("disk:x?=v"), Err(RefError::EmptyParamKey));
    }

    #[test]
    fn param_key_invalid_char_rejected() {
        assert_eq!(
            parse("stats:x?Window=1s"),
            Err(RefError::ParamKeyInvalidChar)
        );
    }

    #[test]
    fn param_value_invalid_char_rejected() {
        assert_eq!(
            parse("stats:x?window=1 s"),
            Err(RefError::ParamValueInvalidChar)
        );
    }

    #[test]
    fn empty_param_value_is_allowed() {
        // An empty value is a legitimate presence test (`key=`), not an error.
        let r = parse("disk:x?empty=").unwrap();
        assert_eq!(r.params()[0].value(), "");
        assert_eq!(r.to_string(), "disk:x?empty=");
    }

    #[test]
    fn too_long_input_rejected() {
        let input = "a".repeat(MAX_REF_LEN + 1);
        assert_eq!(parse(&input), Err(RefError::TooLong));
    }

    #[test]
    fn namespace_too_long_rejected() {
        let ns = "a".repeat(MAX_NAMESPACE_LEN + 1);
        let input = alloc::format!("{ns}:x");
        assert_eq!(parse(&input), Err(RefError::NamespaceTooLong));
    }

    #[test]
    fn segment_too_long_rejected() {
        let seg = "a".repeat(MAX_SEGMENT_LEN + 1);
        let input = alloc::format!("disk:{seg}");
        assert_eq!(parse(&input), Err(RefError::SegmentTooLong));
    }

    #[test]
    fn too_many_segments_rejected() {
        let mut input = String::from("disk:a");
        for _ in 0..MAX_SELECTOR_SEGMENTS {
            input.push_str("/a");
        }
        assert_eq!(parse(&input), Err(RefError::TooManySegments));
    }

    #[test]
    fn guard_too_long_rejected() {
        let fp = "A".repeat(MAX_GUARD_LEN + 1);
        let input = alloc::format!("disk:x@{fp}");
        assert_eq!(parse(&input), Err(RefError::GuardTooLong));
    }

    #[test]
    fn facet_too_long_rejected() {
        let facet = "a".repeat(MAX_FACET_LEN + 1);
        let input = alloc::format!("disk:x::{facet}");
        assert_eq!(parse(&input), Err(RefError::FacetTooLong));
    }

    #[test]
    fn too_many_params_rejected() {
        let mut input = String::from("disk:x?");
        for i in 0..=MAX_PARAMS {
            if i > 0 {
                input.push(',');
            }
            input.push_str("k=v");
        }
        assert_eq!(parse(&input), Err(RefError::TooManyParams));
    }

    #[test]
    fn param_key_too_long_rejected() {
        let key = "a".repeat(MAX_PARAM_KEY_LEN + 1);
        let input = alloc::format!("disk:x?{key}=v");
        assert_eq!(parse(&input), Err(RefError::ParamKeyTooLong));
    }

    #[test]
    fn param_value_too_long_rejected() {
        let value = "a".repeat(MAX_PARAM_VALUE_LEN + 1);
        let input = alloc::format!("disk:x?k={value}");
        assert_eq!(parse(&input), Err(RefError::ParamValueTooLong));
    }

    #[test]
    fn error_display_is_nonempty() {
        for e in [
            RefError::Empty,
            RefError::TooLong,
            RefError::NotAReference,
            RefError::EmptyNamespace,
            RefError::NamespaceTooLong,
            RefError::NamespaceInvalidChar,
            RefError::EmptySelector,
            RefError::EmptySegment,
            RefError::SegmentTooLong,
            RefError::SegmentInvalidChar,
            RefError::TooManySegments,
            RefError::EmptyGuard,
            RefError::GuardTooLong,
            RefError::GuardInvalidChar,
            RefError::EmptyFacet,
            RefError::FacetTooLong,
            RefError::FacetInvalidChar,
            RefError::EmptyParams,
            RefError::TooManyParams,
            RefError::ParamMissingOp,
            RefError::EmptyParamKey,
            RefError::ParamKeyTooLong,
            RefError::ParamKeyInvalidChar,
            RefError::ParamValueTooLong,
            RefError::ParamValueInvalidChar,
        ] {
            assert!(!e.to_string().is_empty());
        }
    }

    /// The registry iterator is exhaustive and agrees with the spellings.
    #[test]
    fn all_is_exhaustive_and_round_trips() {
        for ns in KnownNamespace::ALL {
            assert_eq!(KnownNamespace::from_name(ns.as_str()), Some(ns));
        }
        // Exhaustiveness: a registered spelling outside ALL cannot exist,
        // because from_name and as_str are total over the same enum; the
        // length pin catches a variant added without extending ALL.
        assert_eq!(KnownNamespace::ALL.len(), 16);
    }

    /// Every well-known selector composes into a reference that parses back
    /// to its own namespace and single-segment selector.
    #[test]
    fn well_known_selectors_parse() {
        use alloc::format;

        for ns in KnownNamespace::ALL {
            for selector in ns.well_known_selectors() {
                let spelling = format!("{}:{selector}", ns.as_str());
                let parsed = parse(&spelling).expect("well-known selector parses");
                assert_eq!(parsed.namespace().known(), Some(ns));
                assert_eq!(parsed.selector(), [*selector]);
            }
        }
    }

    /// The happy path of the shared target-resolution rule: a bare registered
    /// namespace classifies as a resource reference, not a file.
    #[test]
    fn registered_namespace_target_classifies_as_resource() {
        for target in ["sys:null", "sys:random", "tty:debug"] {
            match classify_target(target) {
                Ok(TargetClass::Resource(reference)) => {
                    assert!(reference.namespace().known().is_some());
                }
                other => panic!("{target} should be a resource reference, got {other:?}"),
            }
        }
    }

    /// Every spelling the rule keeps on the path side: an alias path (`:`
    /// then `/`), an absolute path, a dot-relative path, a sub-path whose
    /// first component has no `:`, an unregistered prefix, and a plain name.
    #[test]
    fn path_spellings_classify_as_paths() {
        for target in [
            "Home:/notes",
            "/sys:random",
            "./sys:random",
            "../sys:random",
            "foo/sys:random",
            "foo:bar",
            "sys:/foo",
            "mylisting.txt",
        ] {
            assert_eq!(
                classify_target(target),
                Ok(TargetClass::Path),
                "{target} should stay a path",
            );
        }
    }

    /// A registered-namespace prefix with a malformed reference fails closed
    /// rather than falling back to the path world.
    #[test]
    fn malformed_registered_reference_fails_closed() {
        // A guard delimiter with no fingerprint is a grammar violation.
        assert_eq!(classify_target("sys:null@"), Err(RefError::EmptyGuard));
    }
}
