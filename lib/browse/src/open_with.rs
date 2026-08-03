//! The "Open With…" type→bundle association model (`plans/NEW-FILEMANAGER.md`
//! `FM6b`).
//!
//! When the user asks to open a regular file with a chosen application, the
//! file manager offers the installed bundles whose signed `AppInfo` claims the
//! file's type. This module is the **pure model** behind that offer, host-proven
//! without a kernel exactly as the [`Activation`](crate::activate) decision is:
//!
//! * [`applications_for`] derives a file's content type from its filename
//!   extension through the shared content-type registry
//!   ([`media_for_name`]) — the one bridge from a
//!   name (all the VFS listing gives us) to the media-type vocabulary a bundle
//!   declares its associations in. Because that registry is also what the icon
//!   classifier draws from, the applications offered and the glyph shown can
//!   never drift apart. It is a display *hint*, never authority: it decides
//!   which applications are *offered*, and the load gate still verifies and
//!   capability-checks whichever one the user picks.
//! * [`BundleSource`] is the injected enumeration seam — the installed-bundle
//!   analogue of [`DirectorySource`](crate::source). On a running system it is
//!   backed by the app store (each bundle's `AppInfo` MIME table); in tests it
//!   is an in-memory list, so the matching logic is exercised without a kernel.
//! * [`applications_for`] selects the bundles that handle a file's type or any
//!   broader type it is a subclass of
//!   ([`MediaType::parent`](crate::media::MediaType::parent)), so a text editor
//!   declaring `text/plain` is offered for a `.rs` file while an application
//!   declaring `text/x-rust` is offered ahead of it. Bundles that declare the
//!   same type keep the source's order. No match is an **honest empty answer**
//!   — the caller shows a "no application" notice, never a crash and never a
//!   fabricated default.
//!
//! The engine holds no launch authority: it *names* the candidate bundles and
//! *what should happen*; spawning the chosen bundle through the signed load gate
//! stays in the file manager's own capability-checked tail under the user's
//! identity (so the read-only picker, which composes the same engine, never
//! launches). Deciding a file's type here never opens it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::{mime_type_at, AppInfoHeader, Errno};

use crate::media::{ancestry, media_for_name};

/// One installed application and the file types its signed `AppInfo` claims to
/// open — a single "Open With…" candidate.
///
/// The [`mime_types`](Self::mime_types) are the bundle's *own* declared
/// associations (`AppInfo`'s MIME table), never a registry the file manager
/// invents: the manager reads what each bundle claims and offers only those.
/// [`bundle_path`](Self::bundle_path) is the absolute path of the `<Name>.app`
/// directory the caller launches through the ordinary signed load gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppAssociation {
    name: String,
    bundle_path: String,
    mime_types: Vec<String>,
}

impl AppAssociation {
    /// Construct an association from a bundle's display name, the absolute path
    /// of its `<Name>.app` directory, and the MIME types its `AppInfo` declares.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        bundle_path: impl Into<String>,
        mime_types: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            bundle_path: bundle_path.into(),
            mime_types,
        }
    }

    /// The bundle's human-readable name — the "Open With…" menu label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The absolute path of the `<Name>.app` bundle to launch through the
    /// signed load gate.
    #[must_use]
    pub fn bundle_path(&self) -> &str {
        &self.bundle_path
    }

    /// The MIME types the bundle's `AppInfo` declares it can open.
    #[must_use]
    pub fn mime_types(&self) -> &[String] {
        &self.mime_types
    }

    /// Whether this bundle declares an association with `mime`, matched
    /// ASCII-case-insensitively so a type reads the same however it was cased.
    ///
    /// This is the bundle's *own* declaration, tested exactly: a bundle that
    /// declares only `text/plain` does not "handle" `text/x-rust` here.
    /// Offering it for a Rust file is [`applications_for`]'s job, which walks
    /// the subclass chain and asks this question once per broader type.
    #[must_use]
    pub fn handles(&self, mime: &str) -> bool {
        self.mime_types
            .iter()
            .any(|declared| declared.eq_ignore_ascii_case(mime))
    }
}

/// Build an [`AppAssociation`] from a bundle's raw `AppInfo` manifest bytes and
/// its `<Name>.app` directory path.
///
/// This is the pure decode the running-system [`BundleSource`] uses per bundle:
/// it reads the manifest header and the declared MIME table (the same body
/// layout the loader reads) and returns the bundle's name and declared types.
/// It is **fail-closed** — a manifest that does not parse, or whose MIME table
/// is malformed or non-UTF-8, yields `None`, so a corrupt bundle is silently
/// skipped rather than offered on a guess. The MIME set is a display *hint*
/// only: this does **not** verify the manifest signature (the signed load gate
/// does that when the chosen bundle is launched), it only reads what the
/// bundle claims. Keeping the decode here means it is host-tested without a
/// kernel, exactly like the rest of this model.
#[must_use]
pub fn association_from_appinfo(bundle_path: &str, appinfo: &[u8]) -> Option<AppAssociation> {
    let header = AppInfoHeader::from_bytes(appinfo).ok()?;
    let body = appinfo.get(AppInfoHeader::WIRE_LEN..)?;
    let caps = usize::from(header.capability_count);
    let mut mimes = Vec::with_capacity(usize::from(header.mime_count));
    for index in 0..usize::from(header.mime_count) {
        mimes.push(mime_type_at(body, caps, index).ok()?.to_string());
    }
    Some(AppAssociation::new(
        header.bundle_name(),
        bundle_path,
        mimes,
    ))
}

/// The installed-application enumeration seam — the "Open With…" analogue of
/// [`DirectorySource`](crate::source).
///
/// It is the one thing the association model needs from the outside world: the
/// installed bundles and the file types each declares. Keeping it a trait means
/// the matching logic is exhaustively testable against an in-memory list without
/// a kernel, exactly as the browser's directory reads are.
///
/// On a running system the source is backed by the app store, reading each
/// bundle's signed `AppInfo` MIME table under the caller's own identity — the
/// permission decision stays in the store behind the seam, never here.
pub trait BundleSource {
    /// Enumerate the installed applications and their declared file-type
    /// associations.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the app store cannot be
    /// enumerated (for example [`Errno::PermissionDenied`]).
    fn installed_bundles(&mut self) -> Result<Vec<AppAssociation>, Errno>;
}

/// The installed applications that can open a file named `name`, most specific
/// declaration first — the "Open With…" candidate list.
///
/// The file's type is derived by the shared content-type registry
/// ([`media_for_name`]) and named by its media-type spelling
/// ([`MediaType::as_str`](crate::media::MediaType::as_str)), so the association
/// vocabulary is exactly the one the icon classifier draws from — the two can
/// never drift apart.
///
/// A bundle is offered when it [`handles`](AppAssociation::handles) that type
/// **or any broader type it is a subclass of**
/// ([`MediaType::parent`](crate::media::MediaType::parent)): an editor
/// declaring `text/plain` opens a `.rs` file, because Rust source is readable
/// text. Candidates are ordered by how specifically they claim the file — an
/// application declaring the file's own type comes before one declaring an
/// ancestor — and bundles claiming at the same level keep `bundles`'
/// enumeration order, so no existing ordering is disturbed.
///
/// The result is empty — an honest "no application" answer — when the file's
/// type is unrecognised or no installed bundle claims it or any of its broader
/// types; it never falls back to a guessed default.
#[must_use]
pub fn applications_for<'a>(name: &str, bundles: &'a [AppAssociation]) -> Vec<&'a AppAssociation> {
    let Some(media) = media_for_name(name) else {
        return Vec::new();
    };
    let mut ranked: Vec<(usize, &AppAssociation)> = bundles
        .iter()
        .filter_map(|bundle| {
            ancestry(media)
                .position(|claim| bundle.handles(claim.as_str()))
                .map(|distance| (distance, bundle))
        })
        .collect();
    ranked.sort_by_key(|(distance, _)| *distance);
    ranked.into_iter().map(|(_, bundle)| bundle).collect()
}
