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
//! # Two layers: the image's and the administrator's
//!
//! The record is layered exactly as a bundle's `DefaultSettings/` layers
//! under its own store — never copied, never merged into a third document:
//!
//! * the **vendor** layer, [`Enrolment`], is the image's own decision: the
//!   enrolment-governed services PID 1's startup configuration declares. It
//!   cannot come off disk, because no document under `/System` is reliably
//!   readable at the instant the manager must decide what to bring up — the
//!   writable root is not mounted yet and the read-only volume's availability
//!   is a boot-order fact with no userland event;
//! * the **administrator** layer, [`EnrolmentOverride`], is
//!   `/System/Settings/Services/overrides` on the encrypted root, and holds
//!   only the services whose enrolment was *changed* from the image's
//!   default, so a system update shipping a different default takes effect at
//!   once for everything the administrator has not spoken about.
//!
//! [`effective`] folds the pair, so no consumer re-derives the precedence.
//! The split is forced by the volume layout: the whole `/System/Settings`
//! subtree resolves to the writable encrypted root, so nothing there can be
//! read pre-unlock, while the pre-unlock volume is read-only, so nothing there
//! can be written at runtime. A per-user store lives under the user's own
//! `/Users/<u>/Settings/Services/` and is parsed identically.
//!
//! # Fail closed
//!
//! Both documents are untrusted input. Parsing **fails closed**
//! ([`EnrolError`]) on anything malformed — a bad name, a duplicate, an
//! unknown disposition — so a corrupt document yields an error the caller
//! resolves to "nothing is eligible", never a guess. A missing document is
//! the benign empty case. Either way a service that is not positively
//! enrolled never starts.
//!
//! # Enrolment never widens authority
//!
//! [`enrol`] and [`unenrol`] are pure record transforms: they decide only
//! *eligibility*, never authority. The authority boundary is the identity one
//! [`AuthorityScope`](crate::AuthorityScope) already draws on the launch
//! path — a manager may enrol only a service running under an account it is
//! permitted to manage — so there is no second capability-derivation path in
//! the engine to drift from the kernel's authoritative one. The kernel
//! derives `manifest ∩ account-ceiling` from the signed bundle at spawn
//! whatever this record says, so enrolment records a decision and can never
//! grant power.

use alloc::string::String;
use alloc::vec::Vec;

use core::fmt;

use tairix_abi::ServiceEnrolment;
use tairix_util::conf::strip_comment;

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
    /// An override line's disposition word was neither `enabled` nor
    /// `disabled`, or the line carried more than a name and a disposition.
    ServiceEnrolmentInvalid,
}

impl fmt::Display for EnrolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameEmpty => f.write_str("a service name is empty"),
            Self::NameTooLong => f.write_str("a service name is too long"),
            Self::NameInvalid => f.write_str("a service name contains an invalid character"),
            Self::Duplicate => f.write_str("a service is enrolled more than once"),
            Self::NotEnrolled => f.write_str("the service is not enrolled"),
            Self::ServiceEnrolmentInvalid => {
                f.write_str("an override line is not `<service> enabled|disabled`")
            }
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

/// Produce the enrolment with `name` **enabled**.
///
/// Idempotent: enabling an already-enrolled service returns an equal set. The
/// operation is pure — it returns the *new* enrolment for the caller to
/// persist — and it decides eligibility only. It cannot widen authority
/// because it never names one: the kernel derives a service's grant from its
/// signed bundle and its account's ceiling at spawn, and the manager's
/// [`AuthorityScope`](crate::AuthorityScope) decides which accounts it may
/// enrol at all.
///
/// # Errors
///
/// A name defect ([`EnrolError::NameEmpty`] / `NameTooLong` / `NameInvalid`).
pub fn enrol(current: &Enrolment, name: &str) -> Result<Enrolment, EnrolError> {
    validate_service_name(name)?;
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

/// The administrator's override layer: the services whose enrolment differs
/// from the image's [`Enrolment`], and how.
///
/// A service the administrator has not spoken about simply has no entry, so
/// there is no third state meaning "unspecified" — the image's layer decides
/// it.
///
/// Held sorted and unique so the document has one canonical serialisation,
/// and a growable capacity like the vendor layer — the only fixed bound is a
/// single name's length.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnrolmentOverride {
    /// `(service name, disposition)`, validated, unique, ascending by name.
    entries: Vec<(String, ServiceEnrolment)>,
}

impl EnrolmentOverride {
    /// The empty override layer: the image's enrolment stands unchanged.
    ///
    /// The fail-closed resolution of a **missing** document (no administrator
    /// has changed anything, or the encrypted root is not mounted) and of a
    /// **corrupt** one, which is the same answer: obey the signed image.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Parse an override document.
    ///
    /// One `<service> enabled|disabled` per line, `#` comments and blank
    /// lines ignored, mirroring the space-separated grammar the other
    /// `/System/Settings` documents use. Fails closed on a malformed name, an
    /// unknown disposition word, a line with extra words, or a duplicate.
    ///
    /// # Errors
    ///
    /// A name defect, [`EnrolError::ServiceEnrolmentInvalid`] for a malformed
    /// line, or [`EnrolError::Duplicate`] for a repeated service.
    pub fn parse(text: &str) -> Result<Self, EnrolError> {
        let mut entries: Vec<(String, ServiceEnrolment)> = Vec::new();
        for line in text.lines() {
            let content = strip_comment(line).trim();
            if content.is_empty() {
                continue;
            }
            let mut words = content.split_whitespace();
            let (Some(name), Some(word), None) = (words.next(), words.next(), words.next()) else {
                return Err(EnrolError::ServiceEnrolmentInvalid);
            };
            validate_service_name(name)?;
            let disposition =
                ServiceEnrolment::from_name(word).ok_or(EnrolError::ServiceEnrolmentInvalid)?;
            if entries.iter().any(|(n, _)| n == name) {
                return Err(EnrolError::Duplicate);
            }
            entries.push((String::from(name), disposition));
        }
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        Ok(Self { entries })
    }

    /// The administrator's disposition for `name`, or `None` if they have not
    /// spoken about it.
    #[must_use]
    pub fn disposition(&self, name: &str) -> Option<ServiceEnrolment> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| *d)
    }

    /// The recorded entries, ascending by service name.
    #[must_use]
    pub fn entries(&self) -> &[(String, ServiceEnrolment)] {
        &self.entries
    }

    /// Whether the administrator has changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialise to the canonical document text.
    ///
    /// Round-trips with [`parse`](Self::parse).
    #[must_use]
    pub fn to_store_text(&self) -> String {
        let mut out = String::from(
            "# TAIRiX service enrolment overrides. One `<service> enabled|disabled` per line.\n",
        );
        for (name, disposition) in &self.entries {
            out.push_str(name);
            out.push(' ');
            out.push_str(disposition.as_str());
            out.push('\n');
        }
        out
    }
}

/// The enrolment a manager obeys: `vendor` with `overrides` applied.
///
/// The one definition of the precedence, so no consumer re-derives it.
#[must_use]
pub fn effective(vendor: &Enrolment, overrides: &EnrolmentOverride) -> Enrolment {
    let mut names: Vec<String> = vendor
        .names()
        .iter()
        .filter(|n| overrides.disposition(n) != Some(ServiceEnrolment::Disabled))
        .cloned()
        .collect();
    for (name, disposition) in overrides.entries() {
        if disposition.is_enabled() && !names.iter().any(|n| n == name) {
            names.push(name.clone());
        }
    }
    names.sort_unstable();
    Enrolment { names }
}

/// The override layer that makes `desired` the [`effective`] enrolment over
/// `vendor`.
///
/// Self-minimising by construction: a service whose desired state already
/// matches the image's default gets no entry, so the document holds only what
/// was changed and a system update shipping a different default reaches
/// everything the administrator has not spoken about.
#[must_use]
pub fn overrides_for(vendor: &Enrolment, desired: &Enrolment) -> EnrolmentOverride {
    let mut entries: Vec<(String, ServiceEnrolment)> = Vec::new();
    for name in desired.names() {
        if !vendor.is_enabled(name) {
            entries.push((name.clone(), ServiceEnrolment::Enabled));
        }
    }
    for name in vendor.names() {
        if !desired.is_enabled(name) {
            entries.push((name.clone(), ServiceEnrolment::Disabled));
        }
    }
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    EnrolmentOverride { entries }
}

#[cfg(test)]
mod tests {
    use super::{
        effective, enrol, overrides_for, unenrol, EnrolError, Enrolment, EnrolmentOverride,
        ServiceEnrolment, MAX_SERVICE_NAME_LEN,
    };
    use alloc::string::String;
    use alloc::vec::Vec;

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
        let e0 = Enrolment::empty();
        let e1 = enrol(&e0, "netstack").expect("enrols");
        assert!(e1.is_enabled("netstack"));
        // Enabling again changes nothing.
        assert_eq!(e1, enrol(&e1, "netstack").expect("idempotent"));
    }

    #[test]
    fn enrol_rejects_an_invalid_name() {
        assert_eq!(
            enrol(&Enrolment::empty(), "Bad Name"),
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

    #[test]
    fn an_absent_or_corrupt_override_document_leaves_the_image_layer_standing() {
        let vendor = Enrolment::parse("netstack\ntimed\n").expect("parses");
        // Missing document.
        assert_eq!(effective(&vendor, &EnrolmentOverride::empty()), vendor);
        // Corrupt documents fail closed; the caller resolves each to `empty`,
        // which is "obey the signed image".
        for text in [
            "timed\n",                         // no disposition word
            "timed disabled extra\n",          // an extra word
            "timed off\n",                     // an unknown disposition
            "BAD disabled\n",                  // a malformed name
            "timed disabled\ntimed enabled\n", // a duplicate
        ] {
            assert!(
                EnrolmentOverride::parse(text).is_err(),
                "override text should fail closed: {text:?}"
            );
        }
    }

    #[test]
    fn an_override_disables_and_enables_over_the_image_layer() {
        let vendor = Enrolment::parse("netstack\ntimed\n").expect("parses");
        let overrides =
            EnrolmentOverride::parse("timed disabled\nfontd enabled # both directions\n")
                .expect("parses");
        assert_eq!(
            overrides.disposition("timed"),
            Some(ServiceEnrolment::Disabled)
        );
        assert_eq!(
            overrides.disposition("fontd"),
            Some(ServiceEnrolment::Enabled)
        );
        assert_eq!(overrides.disposition("netstack"), None);

        let eff = effective(&vendor, &overrides);
        assert_eq!(names(&eff), ["fontd", "netstack"]);
    }

    #[test]
    fn override_text_round_trips_canonically() {
        let overrides =
            EnrolmentOverride::parse("timed disabled\nfontd enabled\n").expect("parses");
        let text = overrides.to_store_text();
        assert_eq!(
            EnrolmentOverride::parse(&text).expect("canonical text reparses"),
            overrides
        );
        // Ascending by name, one entry per line.
        assert!(text.contains("fontd enabled\n"));
        assert!(text.contains("timed disabled\n"));
        assert!(text.find("fontd").unwrap() < text.find("timed").unwrap());
    }

    #[test]
    fn overrides_for_records_only_what_differs_from_the_image() {
        let vendor = Enrolment::parse("netstack\ntimed\n").expect("parses");

        // Disabling one of the image's services records exactly that.
        let desired = unenrol(&vendor, "timed").expect("removes");
        let overrides = overrides_for(&vendor, &desired);
        assert_eq!(
            overrides.entries(),
            [(String::from("timed"), ServiceEnrolment::Disabled)]
        );
        assert_eq!(effective(&vendor, &overrides), desired);

        // Re-enabling it empties the document rather than pinning the
        // default: a later image that ships `timed` disabled must then be
        // obeyed, because the administrator is no longer speaking about it.
        let back = enrol(&desired, "timed").expect("enrols");
        assert!(overrides_for(&vendor, &back).is_empty());

        // Enabling something the image does not ship records an Enabled entry.
        let extra = enrol(&vendor, "fontd").expect("enrols");
        assert_eq!(
            overrides_for(&vendor, &extra).entries(),
            [(String::from("fontd"), ServiceEnrolment::Enabled)]
        );
    }

    #[test]
    fn effective_and_overrides_for_are_inverse_over_every_pair() {
        // Whatever the administrator wants, the derived document reproduces
        // it exactly over the image layer — the property both PID 1's boot
        // read and its control path rely on.
        let vendor = Enrolment::parse("a\nb\nc\n").expect("parses");
        for wanted in [
            "",
            "a\n",
            "b\nc\n",
            "a\nb\nc\n",
            "d\n",
            "a\nd\n",
            "b\nd\ne\n",
        ] {
            let desired = Enrolment::parse(wanted).expect("parses");
            let overrides = overrides_for(&vendor, &desired);
            assert_eq!(
                effective(&vendor, &overrides),
                desired,
                "round trip failed for {wanted:?}"
            );
            // The derived document is itself canonical.
            assert_eq!(
                EnrolmentOverride::parse(&overrides.to_store_text()).expect("reparses"),
                overrides
            );
        }
    }
}
