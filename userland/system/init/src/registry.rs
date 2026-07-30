//! Service discovery and the **enrolment registry** (`plans/NEW-SERVICEMANAGER.md`
//! §2, §3.1).
//!
//! A service, unlike a driver, has no natural activation gate: a driver's is
//! "the hardware is physically present and matched", but dropping a signed
//! service bundle on disk must **not** make a live service appear — that is
//! an ambient-authority-shaped risk. The lifecycle is therefore split into
//! three distinct steps:
//!
//! 1. **Discovery** — what bundles exist (a scan of `/System/Services`,
//!    reusing the same store walk drivers use; not this module's job).
//! 2. **Registration / enablement** — is a discovered bundle *eligible* to
//!    auto-start or be on-demand-activated. That decision is an explicit,
//!    recorded, integrity-protected entry in the **registration store**,
//!    never implied by presence. This module owns that decision.
//! 3. **Activation** — actually starting an eligible service (the manager's
//!    job, [`Init::register_enrolled`](crate::Init::register_enrolled) and
//!    the bring-up engine).
//!
//! # What the store holds — and what it does not
//!
//! The store holds only **enrolment records**: a set of enabled service
//! names keyed to the bundles they name. It is deliberately *not* a second
//! copy of a service's unit metadata (restart policy, activation mode,
//! linger, dependencies, readiness conditions, rlimits) — that lives in the
//! service's **signed `AppInfo` bundle manifest**, so tampering is a load
//! refusal rather than a silent behaviour change. Duplicating unit metadata
//! into a separately-writable store would be both the duplication the
//! charter forbids and a place an attacker could raise a service's authority.
//!
//! The system store is read from `/System/Settings/Services/enabled`
//! (`tairix_abi::driver_store::SystemConfigFile::SystemServices`) off the
//! always-mounted read-only `/System` volume, through the same confined,
//! fail-closed pre-unlock read path the device manager already uses for its
//! configuration — no new read primitive. A per-user store lives under the
//! user's own `/Users/<u>/Settings/Services/` and is parsed identically.
//!
//! # Fail closed
//!
//! The store text is untrusted input. Parsing **fails closed**
//! ([`EnrolError`]) on anything malformed — a bad name, a duplicate — so a
//! corrupt store yields an error the caller resolves to "nothing is
//! eligible", never a guess. A missing store is the benign empty case: no
//! service is enabled. Either way a service that is not positively enrolled
//! never starts.
//!
//! # Enrolment never widens authority
//!
//! [`enrol`] takes the enroller's capability ceiling and the service's
//! signed manifest and **refuses** ([`EnrolError::CapabilityEscalation`]) to
//! enable a service whose manifest requests authority the enroller does not
//! hold. A user enabling a service in their own scope can therefore never
//! make it eligible to run with more authority than the user has, and the
//! manager still intersects the grant with the system authority at start
//! regardless — enrolment records a decision, it never grants power.

use alloc::string::String;
use alloc::vec::Vec;

use core::fmt;

use tairix_abi::Errno;
use tairix_caps::CapabilitySet;
use tairix_util::conf::strip_comment;

use crate::service::decode_manifest_capabilities;

/// Maximum length, in bytes, of a single service name.
///
/// A validation bound on one identifier (never a cap on *how many* services
/// may be enrolled — that set is a growable capacity, not a fixed ceiling):
/// a service name is a short bundle identifier, and an over-long one is a
/// packaging defect, not a workload. Fail closed
/// ([`EnrolError::NameTooLong`]) rather than truncate.
pub const MAX_SERVICE_NAME_LEN: usize = 64;

/// Why an enrolment store text, or an [`enrol`] / [`unenrol`] request, was
/// refused.
///
/// Every variant is a fail-closed refusal: the operation records nothing and
/// changes nothing, so a malformed store or request can never make a
/// surprising service eligible.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EnrolError {
    /// A service name was empty.
    NameEmpty,
    /// A service name exceeded [`MAX_SERVICE_NAME_LEN`].
    NameTooLong,
    /// A service name contained a byte outside the permitted set (a
    /// lowercase-ASCII bundle identifier: `[a-z0-9]` first, then
    /// `[a-z0-9._-]`). Rejecting anything else keeps a path-traversal- or
    /// case-collision-shaped name out of the store.
    NameInvalid,
    /// The store text names the same service more than once.
    Duplicate,
    /// [`unenrol`] named a service that is not currently enrolled; nothing
    /// changed.
    NotEnrolled,
    /// [`enrol`] refused because the service's manifest requests a
    /// capability the enroller's ceiling does not hold. Enabling it would
    /// make it eligible to run with authority the enroller lacks, so it is
    /// refused rather than recorded.
    CapabilityEscalation,
    /// [`enrol`] could not decode the service's signed manifest into a
    /// requested capability set; the wrapped [`Errno`] is the decode error.
    ManifestInvalid(Errno),
}

impl fmt::Display for EnrolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameEmpty => f.write_str("a service name is empty"),
            Self::NameTooLong => f.write_str("a service name is too long"),
            Self::NameInvalid => f.write_str("a service name contains an invalid character"),
            Self::Duplicate => f.write_str("a service is enrolled more than once"),
            Self::NotEnrolled => f.write_str("the service is not enrolled"),
            Self::CapabilityEscalation => {
                f.write_str("the service requests authority the enroller does not hold")
            }
            Self::ManifestInvalid(e) => write!(f, "the service manifest is invalid: {e}"),
        }
    }
}

/// Validate a service name: a non-empty, lowercase-ASCII bundle identifier of
/// at most [`MAX_SERVICE_NAME_LEN`] bytes whose first byte is `[a-z0-9]` and
/// whose remaining bytes are `[a-z0-9._-]`.
///
/// The strict alphabet is a security control, not cosmetics: because a name
/// can neither start with `.` nor contain `/`, a store entry can never be a
/// `..` or path-traversal-shaped token, and because it is lowercase-only two
/// entries can never collide by case.
///
/// # Errors
///
/// [`EnrolError::NameEmpty`], [`EnrolError::NameTooLong`], or
/// [`EnrolError::NameInvalid`] for the respective defect.
pub fn validate_service_name(name: &str) -> Result<(), EnrolError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Err(EnrolError::NameEmpty);
    }
    if bytes.len() > MAX_SERVICE_NAME_LEN {
        return Err(EnrolError::NameTooLong);
    }
    for (i, &b) in bytes.iter().enumerate() {
        let ok = b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || (i > 0 && matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(EnrolError::NameInvalid);
        }
    }
    Ok(())
}

/// The parsed enrolment record of one scope: the set of service names that
/// are eligible to be brought up.
///
/// The names are validated, unique, and held in sorted order so the store
/// has one canonical serialisation (a change to the set is a minimal,
/// order-stable diff). The set is a growable capacity — there is no
/// fixed cap on how many services may be enrolled.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Enrolment {
    /// Enabled service names, validated, unique, ascending.
    names: Vec<String>,
}

impl Enrolment {
    /// The empty enrolment: nothing is enabled.
    ///
    /// This is the fail-closed resolution of both a **missing** store (an
    /// unprovisioned or freshly-installed system) and a **corrupt** store
    /// whose [`parse`](Self::parse) returned an error: in every case no
    /// service is eligible, never a guess.
    #[must_use]
    pub const fn empty() -> Self {
        Self { names: Vec::new() }
    }

    /// Parse an enrolment store text.
    ///
    /// The grammar mirrors the startup config: a sequence of lines, `#`
    /// begins a comment to end of line, blank and comment-only lines are
    /// ignored, and every other line is a single service name (surrounding
    /// whitespace tolerated). Fails closed on any malformed name or a
    /// duplicate, so a corrupt store never yields a partial, surprising set.
    ///
    /// # Errors
    ///
    /// [`EnrolError::NameEmpty`] / [`EnrolError::NameTooLong`] /
    /// [`EnrolError::NameInvalid`] for a malformed name, or
    /// [`EnrolError::Duplicate`] if a name appears twice.
    pub fn parse(text: &str) -> Result<Self, EnrolError> {
        let mut names: Vec<String> = Vec::new();
        for line in text.lines() {
            let content = strip_comment(line).trim();
            if content.is_empty() {
                continue;
            }
            validate_service_name(content)?;
            if names.iter().any(|n| n == content) {
                return Err(EnrolError::Duplicate);
            }
            names.push(String::from(content));
        }
        names.sort_unstable();
        Ok(Self { names })
    }

    /// Whether the named service is enrolled (eligible to be brought up).
    #[must_use]
    pub fn is_enabled(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// The enrolled service names, sorted ascending.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Number of enrolled services.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether no service is enrolled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Serialise to the canonical store text: a short generated-file header
    /// comment followed by one enrolled service name per line, ascending.
    ///
    /// Round-trips with [`parse`](Self::parse): parsing the output yields an
    /// equal [`Enrolment`]. Trusted tooling and the control path write this
    /// text back to the registration store.
    #[must_use]
    pub fn to_store_text(&self) -> String {
        let mut out =
            String::from("# TAIRiX service enrolment record. One enabled service per line.\n");
        for name in &self.names {
            out.push_str(name);
            out.push('\n');
        }
        out
    }
}

/// Produce the enrolment with `name` **enabled**, given the enroller's
/// capability `ceiling` and the service's signed `manifest`.
///
/// Idempotent: enabling an already-enrolled service returns an equal set. The
/// operation is pure — it returns the *new* enrolment for the caller to write
/// back through the appropriate trusted-path store — and it can never widen
/// authority: it refuses ([`EnrolError::CapabilityEscalation`]) to enable a
/// service whose manifest requests authority beyond `ceiling`.
///
/// `accepted_abi_version` is the ABI version the manifest must target (the
/// same the manager accepts, so a manifest that would be refused at start is
/// refused at enrol time too).
///
/// # Errors
///
/// A name defect ([`EnrolError::NameEmpty`] / `NameTooLong` / `NameInvalid`),
/// [`EnrolError::ManifestInvalid`] if the manifest does not decode, or
/// [`EnrolError::CapabilityEscalation`] if the request exceeds `ceiling`.
pub fn enrol(
    current: &Enrolment,
    name: &str,
    manifest: &[u8],
    accepted_abi_version: u32,
    ceiling: &CapabilitySet,
) -> Result<Enrolment, EnrolError> {
    validate_service_name(name)?;
    let requested = decode_manifest_capabilities(manifest, accepted_abi_version)
        .map_err(EnrolError::ManifestInvalid)?;
    if !requested.is_subset_of(ceiling) {
        return Err(EnrolError::CapabilityEscalation);
    }
    let mut names = current.names.clone();
    if !names.iter().any(|n| n == name) {
        names.push(String::from(name));
        names.sort_unstable();
    }
    Ok(Enrolment { names })
}

/// Produce the enrolment with `name` **disabled**.
///
/// The operation is pure — it returns the new enrolment for the caller to
/// write back. Disabling requires no capability check (removing eligibility
/// only ever narrows authority) but does fail closed if the service was not
/// enrolled, so a control tool reports honestly that nothing changed.
///
/// # Errors
///
/// A name defect, or [`EnrolError::NotEnrolled`] if `name` is not currently
/// enrolled.
pub fn unenrol(current: &Enrolment, name: &str) -> Result<Enrolment, EnrolError> {
    validate_service_name(name)?;
    if !current.is_enabled(name) {
        return Err(EnrolError::NotEnrolled);
    }
    let names = current
        .names
        .iter()
        .filter(|n| n.as_str() != name)
        .cloned()
        .collect();
    Ok(Enrolment { names })
}

#[cfg(test)]
mod tests {
    use super::{enrol, unenrol, EnrolError, Enrolment, MAX_SERVICE_NAME_LEN};
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::{
        CapabilityId, ManifestHeader, ABI_VERSION_CURRENT, MANIFEST_MAGIC, SYSCALL_TABLE_HASH_LEN,
    };
    use tairix_caps::CapabilitySet;

    /// A syntactically valid manifest requesting `requested`, at the current
    /// ABI version (mirrors the manager's test helper).
    fn manifest(requested: &[CapabilityId]) -> Vec<u8> {
        let header = ManifestHeader {
            magic: MANIFEST_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: u16::try_from(requested.len()).unwrap(),
            reserved0: 0,
            syscall_table_hash: [0u8; SYSCALL_TABLE_HASH_LEN],
            signer_pubkey: [0u8; 32],
            signature: [0u8; 64],
        };
        let mut out = header.to_le_bytes().to_vec();
        for cap in requested {
            out.extend_from_slice(&cap.as_u16().to_le_bytes());
        }
        out
    }

    fn cap_set(list: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for cap in list {
            set.insert(*cap);
        }
        set
    }

    fn names(e: &Enrolment) -> Vec<&str> {
        e.names().iter().map(String::as_str).collect()
    }

    #[test]
    fn parse_collects_enabled_names_sorted_and_ignores_comments() {
        let text = "\
# the service enrolment record
netstack
sysinfod   # inline comment tolerated

devmgr
";
        let e = Enrolment::parse(text).expect("well-formed store parses");
        assert_eq!(names(&e), ["devmgr", "netstack", "sysinfod"]);
        assert!(e.is_enabled("netstack"));
        assert!(!e.is_enabled("fontd"));
        assert_eq!(e.len(), 3);
    }

    #[test]
    fn an_empty_or_comment_only_store_enrols_nothing() {
        let e = Enrolment::parse("# nothing enabled\n\n   \n").expect("parses");
        assert!(e.is_empty());
        // The missing-store case resolves to the same empty enrolment.
        assert!(Enrolment::empty().is_empty());
    }

    #[test]
    fn a_corrupt_store_fails_closed_and_enrols_nothing() {
        // A name with an invalid character rejects the whole store, so the
        // caller resolves it to the empty (nothing-eligible) set — never a
        // partial guess.
        assert_eq!(
            Enrolment::parse("netstack\nBAD NAME\n"),
            Err(EnrolError::NameInvalid),
        );
        assert_eq!(
            Enrolment::parse("a\n../etc\n"),
            Err(EnrolError::NameInvalid)
        );
        assert_eq!(Enrolment::parse("dup\ndup\n"), Err(EnrolError::Duplicate),);
    }

    #[test]
    fn name_validation_is_strict_and_fails_closed() {
        use super::validate_service_name;
        assert_eq!(validate_service_name(""), Err(EnrolError::NameEmpty));
        assert_eq!(
            validate_service_name(&"x".repeat(MAX_SERVICE_NAME_LEN + 1)),
            Err(EnrolError::NameTooLong),
        );
        // A leading dot (path traversal shape) is refused: the first byte
        // must be alphanumeric.
        assert_eq!(
            validate_service_name(".hidden"),
            Err(EnrolError::NameInvalid)
        );
        assert_eq!(validate_service_name("a/b"), Err(EnrolError::NameInvalid));
        assert_eq!(validate_service_name("Upper"), Err(EnrolError::NameInvalid));
        assert_eq!(validate_service_name("net-stack_2.0"), Ok(()));
    }

    #[test]
    fn to_store_text_round_trips() {
        let e = Enrolment::parse("sysinfod\nnetstack\ndevmgr\n").expect("parses");
        let text = e.to_store_text();
        let reparsed = Enrolment::parse(&text).expect("canonical text reparses");
        assert_eq!(e, reparsed);
        assert_eq!(names(&reparsed), ["devmgr", "netstack", "sysinfod"]);
    }

    #[test]
    fn enrol_is_idempotent_and_adds_a_service() {
        let ceiling = cap_set(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let m = manifest(&[CapabilityId::NET_RAW]);
        let e0 = Enrolment::empty();
        let e1 = enrol(&e0, "netstack", &m, ABI_VERSION_CURRENT, &ceiling).expect("enrols");
        assert!(e1.is_enabled("netstack"));
        // Enabling again changes nothing.
        let e2 = enrol(&e1, "netstack", &m, ABI_VERSION_CURRENT, &ceiling).expect("idempotent");
        assert_eq!(e1, e2);
    }

    #[test]
    fn enrol_never_widens_authority() {
        // The enroller holds only FS_MOUNT, but the service's manifest
        // requests NET_RAW — enabling it would make it eligible to run with
        // authority the enroller lacks, so enrol refuses, fail closed.
        let enroller = cap_set(&[CapabilityId::FS_MOUNT]);
        let greedy = manifest(&[CapabilityId::NET_RAW]);
        assert_eq!(
            enrol(
                &Enrolment::empty(),
                "spy",
                &greedy,
                ABI_VERSION_CURRENT,
                &enroller
            ),
            Err(EnrolError::CapabilityEscalation),
        );
        // A service within the ceiling is fine.
        let ok = manifest(&[CapabilityId::FS_MOUNT]);
        assert!(enrol(
            &Enrolment::empty(),
            "fsd",
            &ok,
            ABI_VERSION_CURRENT,
            &enroller
        )
        .expect("within ceiling")
        .is_enabled("fsd"));
    }

    #[test]
    fn enrol_rejects_an_undecodable_manifest_fail_closed() {
        let ceiling = cap_set(&[CapabilityId::FS_MOUNT]);
        // Empty bytes are not a valid manifest header.
        assert!(matches!(
            enrol(
                &Enrolment::empty(),
                "svc",
                &[],
                ABI_VERSION_CURRENT,
                &ceiling
            ),
            Err(EnrolError::ManifestInvalid(_)),
        ));
    }

    #[test]
    fn enrol_rejects_an_invalid_name_before_touching_the_manifest() {
        let ceiling = cap_set(&[CapabilityId::FS_MOUNT]);
        assert_eq!(
            enrol(
                &Enrolment::empty(),
                "Bad Name",
                &[],
                ABI_VERSION_CURRENT,
                &ceiling
            ),
            Err(EnrolError::NameInvalid),
        );
    }

    #[test]
    fn unenrol_removes_a_service_and_fails_closed_on_absent() {
        let e = Enrolment::parse("a\nb\nc\n").expect("parses");
        let after = unenrol(&e, "b").expect("removes");
        assert_eq!(names(&after), ["a", "c"]);
        assert!(!after.is_enabled("b"));
        // Disabling a service that is not enrolled fails closed.
        assert_eq!(unenrol(&e, "z"), Err(EnrolError::NotEnrolled));
    }
}
