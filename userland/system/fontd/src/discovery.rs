//! Discovery of the on-disk font store: turning the family directories under
//! `/System/Fonts` into the resolved families [`crate::FontService`] serves
//! from.
//!
//! Discovery only ever *lists* the store and reads each family's small
//! `FontFamily` manifest text — it never reads a face's bytes. A face is
//! read once, lazily, the first time a request actually needs it
//! ([`FaceLoad`]), so a session that never draws a script never pays for the
//! face that covers it. [`FontStore`] is the seam that makes the scan itself
//! host-testable: a host test drives it from an in-memory fixture, the `Run`
//! binary drives it from the real filesystem through `tairix-rt`.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::font_ipc::{FamilyKey, FONT_MAX_FAMILIES};
use tairix_abi::Errno;
use tairix_fontface::FamilyManifest;
use tairix_log::{log, Event, Level, Sink};

use crate::events::FAMILY_SKIPPED;
use crate::service::{FaceCache, FamilyRuntime, FontService};
use crate::GlyphCache;

/// Supplies one face's raw bytes on first use.
///
/// A face's bytes are read once, on first use, and retained for the
/// service's life; a session that never touches a family's script never pays
/// for its face. A host test hands back bytes it already holds
/// ([`Preloaded`]); the freestanding service re-opens the exact stored path.
pub trait FaceLoad<'a> {
    /// Obtain the face's bytes, reading them if this is the first call.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the underlying read raises.
    fn load(&mut self) -> Result<&'a [u8], Errno>;
}

/// A [`FaceLoad`] over bytes the caller already holds: the host-test loader.
pub struct Preloaded<'a>(pub &'a [u8]);

impl<'a> FaceLoad<'a> for Preloaded<'a> {
    fn load(&mut self) -> Result<&'a [u8], Errno> {
        Ok(self.0)
    }
}

/// Read access to the on-disk font store the service discovers families
/// from.
///
/// Abstracted so the discovery and resolution logic in this crate is
/// host-testable against an in-memory fixture; the `Run` binary's own
/// implementation reads through `tairix-rt`.
pub trait FontStore<'a> {
    /// The store's family directory names, in whatever order the
    /// implementation happens to list them — [`discover`] sorts them before
    /// use, so the scan is deterministic regardless of listing order.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the store itself could not even be opened with. This is
    /// the one failure [`discover`] treats as immediately fatal, since no
    /// family can possibly be found without it.
    fn family_dirs(&mut self) -> Result<Vec<String>, Errno>;

    /// The `FontFamily` manifest text of family directory `dir`, or `None`
    /// when the directory carries no readable manifest.
    fn read_manifest(&mut self, dir: &str) -> Option<String>;

    /// A lazy loader for face file `face` inside family directory `dir`.
    fn face_loader(&mut self, dir: &str, face: &str) -> Box<dyn FaceLoad<'a> + 'a>;
}

/// Record a non-fatal "this family was skipped" warning.
fn log_skip(sink: &dyn Sink, message: &str) {
    let _ = log(
        sink,
        &Event {
            level: Level::Warn,
            id: FAMILY_SKIPPED,
            message,
            fields: &[],
        },
    );
}

/// Discover every usable family in `store` and build the [`FontService`]
/// that serves them.
///
/// The scan is bounded to [`FONT_MAX_FAMILIES`] directories and sorted by
/// directory name first, so the discovered order — and hence the families
/// reply [`crate::FontService`] serves — is deterministic regardless of the
/// store's own listing order. A directory whose name is not a valid family
/// key, that carries no manifest, or whose manifest does not parse is
/// skipped with a logged warning; it is never fatal on its own.
///
/// # Errors
///
/// [`Errno::NotFound`] when the store itself could not be listed, or when
/// not a single family was usable — a store with no usable family cannot
/// serve text at all, so this is the one fatal startup error.
pub fn discover<'a>(
    store: &mut impl FontStore<'a>,
    cache: GlyphCache,
    sink: &dyn Sink,
) -> Result<FontService<'a>, Errno> {
    let mut dirs = store.family_dirs().map_err(|_| Errno::NotFound)?;
    dirs.sort();

    let mut families = Vec::new();
    for dir in dirs.into_iter().take(FONT_MAX_FAMILIES) {
        let Ok(key) = FamilyKey::new(&dir) else {
            log_skip(
                sink,
                "fontd: a /System/Fonts entry is not a valid family key",
            );
            continue;
        };
        let Some(text) = store.read_manifest(&dir) else {
            log_skip(
                sink,
                "fontd: a /System/Fonts family carries no readable manifest",
            );
            continue;
        };
        let Ok(manifest) = FamilyManifest::parse(key, &text) else {
            log_skip(sink, "fontd: a /System/Fonts family manifest is malformed");
            continue;
        };
        let faces = manifest
            .faces()
            .iter()
            .map(|name| FaceCache::new(store.face_loader(&dir, name)))
            .collect();
        families.push(FamilyRuntime::new(
            key,
            manifest.label().to_string(),
            manifest.selectable_kind(),
            faces,
            manifest.fallback(),
        ));
    }

    if families.is_empty() {
        return Err(Errno::NotFound);
    }
    Ok(FontService::from_families(families, cache))
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! An in-memory [`FontStore`] fixture shared by this module's own
    //! discovery tests and the [`crate::service`] resolution/rendering
    //! tests, so both are exercised against the same small, fast, and
    //! deterministic store rather than each inventing its own.

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use tairix_abi::Errno;

    use super::{Box, FaceLoad, FontStore, Preloaded};

    /// One family directory the fixture store holds: its manifest text and
    /// its named faces' bytes.
    pub(crate) struct MemoryFamily<'a> {
        pub(crate) manifest: &'a str,
        pub(crate) faces: Vec<(&'a str, &'a [u8])>,
    }

    /// An in-memory store: directory name to [`MemoryFamily`].
    pub(crate) struct MemoryStore<'a> {
        pub(crate) dirs: Vec<(&'a str, MemoryFamily<'a>)>,
    }

    impl<'a> FontStore<'a> for MemoryStore<'a> {
        fn family_dirs(&mut self) -> Result<Vec<String>, Errno> {
            Ok(self
                .dirs
                .iter()
                .map(|&(name, _)| name.to_string())
                .collect())
        }

        fn read_manifest(&mut self, dir: &str) -> Option<String> {
            self.dirs
                .iter()
                .find(|&&(name, _)| name == dir)
                .map(|(_, family)| family.manifest.to_string())
        }

        fn face_loader(&mut self, dir: &str, face: &str) -> Box<dyn FaceLoad<'a> + 'a> {
            let bytes = self
                .dirs
                .iter()
                .find(|&&(name, _)| name == dir)
                .and_then(|(_, family)| {
                    family
                        .faces
                        .iter()
                        .find(|&&(name, _)| name == face)
                        .map(|&(_, bytes)| bytes)
                })
                .unwrap_or(&[]);
            Box::new(Preloaded(bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use tairix_abi::font_ipc::FamilyKind;
    use tairix_log::DiscardSink;
    use tairix_reclaim::{PressureBand, ReportedPressure};

    use super::discover;
    use super::fixtures::{MemoryFamily, MemoryStore};
    use crate::service::glyph_cache;

    /// A machine with plenty of RAM, so a test that is not about the bound
    /// gets a cache that comfortably holds what it asks for.
    const ROOMY_MACHINE_BYTES: u64 = 64 << 30;

    /// A byte-budgeted cache big enough for any of this module's tests, over
    /// a gauge a host test has nowhere to send pressure changes to.
    fn roomy_cache() -> crate::GlyphCache {
        static SINK: DiscardSink = DiscardSink;
        static GAUGE: ReportedPressure = ReportedPressure::unknown();
        GAUGE.report(PressureBand::Normal);
        glyph_cache(ROOMY_MACHINE_BYTES, &GAUGE, &SINK)
    }

    /// The committed `mono` face, small enough to embed directly in a test.
    const MONO_FACE: &[u8] = include_bytes!("../../../../lib/font/assets/mono/Inconsolata-EX.ttf");

    #[test]
    fn a_store_with_no_manifest_anywhere_is_a_fatal_startup_error() {
        let mut store = MemoryStore { dirs: Vec::new() };
        assert!(discover(&mut store, roomy_cache(), &DiscardSink).is_err());
    }

    #[test]
    fn an_unreadable_or_malformed_family_is_skipped_not_fatal() {
        let mut store = MemoryStore {
            dirs: vec![
                (
                    "not a key!",
                    MemoryFamily {
                        manifest: "label = Bad\nkind = proportional\nface = A.ttf\n",
                        faces: Vec::new(),
                    },
                ),
                (
                    "broken",
                    MemoryFamily {
                        manifest: "kind = proportional\n", // no label, no face
                        faces: Vec::new(),
                    },
                ),
                (
                    "mono",
                    MemoryFamily {
                        manifest: "label = Mono\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
            ],
        };
        let service =
            discover(&mut store, roomy_cache(), &DiscardSink).expect("one usable family is enough");
        assert_eq!(service.family_count(), 1);
    }

    #[test]
    fn discovery_ignores_a_directorys_listing_order() {
        let mut a_first = MemoryStore {
            dirs: vec![
                (
                    "mono",
                    MemoryFamily {
                        manifest: "label = Mono\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
                (
                    "zzz",
                    MemoryFamily {
                        manifest: "label = Zzz\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
            ],
        };
        let mut z_first = MemoryStore {
            dirs: vec![
                (
                    "zzz",
                    MemoryFamily {
                        manifest: "label = Zzz\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
                (
                    "mono",
                    MemoryFamily {
                        manifest: "label = Mono\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
            ],
        };
        let one = discover(&mut a_first, roomy_cache(), &DiscardSink).expect("discovers");
        let two = discover(&mut z_first, roomy_cache(), &DiscardSink).expect("discovers");
        assert_eq!(one.family_labels(), two.family_labels());
    }

    #[test]
    fn a_fallback_role_family_is_discovered_but_never_selectable() {
        let mut store = MemoryStore {
            dirs: vec![
                (
                    "mono",
                    MemoryFamily {
                        manifest: "label = Mono\nkind = monospace\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
                (
                    "fallback-only",
                    MemoryFamily {
                        manifest:
                            "label = Fallback Set\nkind = fallback\nface = Inconsolata-EX.ttf\n",
                        faces: vec![("Inconsolata-EX.ttf", MONO_FACE)],
                    },
                ),
            ],
        };
        let service = discover(&mut store, roomy_cache(), &DiscardSink).expect("discovers");
        assert_eq!(service.family_count(), 2, "both directories are discovered");
        let mut reply = vec![0u8; tairix_abi::font_ipc::FONT_MAX_FAMILIES_REPLY];
        let n = service.families_reply(&mut reply).expect("encodes");
        let list = tairix_abi::font_ipc::decode_families_reply(&reply[..n]).expect("decodes");
        assert_eq!(
            list.entries().len(),
            1,
            "the fallback-only set is never offered"
        );
        assert_eq!(list.entries()[0].kind, FamilyKind::Monospace);
    }
}
