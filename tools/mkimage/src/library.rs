//! Derivation of the shipped program-library catalog from the planted
//! application bundles.
//!
//! The machine-wide catalog the desktop's Program Library reads
//! (`tairix_proglib::MACHINE_LIBRARY_PATH`) is **discovered** from the
//! signed `AppInfo` manifests the image actually plants — never a
//! hand-maintained list: a bundle whose manifest declares a library folder
//! is catalogued under it, and every other bundle stays out. The document
//! is rendered through the one `tairix_proglib` engine, so the shipped
//! store and its readers cannot drift, and adding an application to a
//! fresh image's library is dropping its manifest opt-in on disk
//! (`plans/NEW-TASKBAR.md` T3).

use tairix_abi::{AppInfoHeader, BundleEntry, ProgramKind};
use tairix_proglib::{render, BundlePath, Catalog, DisplayName, EntryId, IconAsset, LibraryEntry};

use crate::MkimageError;

/// Derive the machine-wide program-library catalog document from the
/// `/System`-volume-relative bundle files an image plants.
///
/// Scans both program stores' `<store>/<bundle>.app/AppInfo` manifests — a
/// user launches a command and a graphical application alike, so either may
/// list itself — catalogues every bundle whose manifest declares a library
/// folder, and renders the canonical document. Bundles without a listing,
/// files that are not a bundle manifest, and the non-program stores
/// (`Services`, fonts) are ignored.
///
/// # Errors
///
/// [`MkimageError::LibraryCatalog`] on a manifest that does not decode, a
/// field the catalog model refuses, or two bundles claiming one library
/// identifier — the build fails closed rather than shipping a catalog that
/// misleads the launcher.
pub fn library_catalog(apps: &[(&[&[u8]], &[u8])]) -> Result<String, MkimageError> {
    let mut catalog = Catalog::new();
    for (components, bytes) in apps {
        let parts: &[&[u8]] = components;
        let &[store_dir, bundle_dir, leaf] = parts else {
            continue;
        };
        if leaf != BundleEntry::AppInfo.as_str().as_bytes() {
            continue;
        }
        let Some(kind) = ProgramKind::ALL
            .into_iter()
            .find(|kind| kind.is_searched() && kind.store_dir().as_bytes() == store_dir)
        else {
            continue;
        };
        let bundle_dir = core::str::from_utf8(bundle_dir).map_err(|_| {
            MkimageError::LibraryCatalog("bundle directory name is not UTF-8".into())
        })?;
        let fail = |detail: &dyn core::fmt::Display| {
            MkimageError::LibraryCatalog(format!("{bundle_dir}: {detail}"))
        };

        // The image's own composed manifest must decode; anything else is
        // a build defect, not a bundle to skip.
        let header = AppInfoHeader::from_bytes(bytes)
            .map_err(|e| fail(&format_args!("manifest does not decode ({e:?})")))?;
        let Some(category) = header.library_category() else {
            continue;
        };

        let id = EntryId::new(header.bundle_id()).map_err(|e| fail(&e))?;
        let name = DisplayName::new(header.bundle_name()).map_err(|e| fail(&e))?;
        let path =
            BundlePath::new(&format!("{}/{bundle_dir}", kind.store())).map_err(|e| fail(&e))?;
        let icon = match header.library_icon() {
            Some(asset) => Some(IconAsset::new(asset).map_err(|e| fail(&e))?),
            None => None,
        };

        let entry = LibraryEntry::new(id, name, path, category, icon);
        if catalog.insert(entry).map_err(|e| fail(&e))?.is_some() {
            return Err(fail(&"duplicate library identifier"));
        }
    }
    Ok(render(&catalog))
}

/// Test-only decodable wire manifest for `name`, listed under `listing`
/// with `icon` when given — shared by this module's tests and the
/// whole-image fixtures in `crate::lib`, which must plant manifests the
/// catalog derivation accepts (a planted manifest that does not decode
/// fails the build closed by design).
#[cfg(test)]
pub(crate) fn test_manifest(
    name: &str,
    listing: Option<tairix_abi::LibraryCategory>,
    icon: Option<&str>,
) -> Vec<u8> {
    use tairix_abi::{
        ABI_VERSION_CURRENT, APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX,
        LIBRARY_ICON_MAX,
    };

    fn inline<const N: usize>(text: &str) -> ([u8; N], u8) {
        let mut buf = [0u8; N];
        buf[..text.len()].copy_from_slice(text.as_bytes());
        (buf, u8::try_from(text.len()).expect("fits"))
    }
    let (id_buf, id_len) = inline::<BUNDLE_ID_MAX>(&format!("os.tairix.{name}"));
    let (name_buf, name_len) = inline::<BUNDLE_NAME_MAX>(name);
    let (version, version_len) = inline::<BUNDLE_VERSION_MAX>("1.0");
    let (icon_buf, icon_len) = inline::<LIBRARY_ICON_MAX>(icon.unwrap_or(""));
    AppInfoHeader {
        magic: APPINFO_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: 0,
        capability_count: 0,
        mime_count: 0,
        id_len,
        name_len,
        version_len,
        purpose_len: 0,
        author_len: 0,
        library_icon_len: icon_len,
        library: tairix_abi::LibraryCategory::to_wire(listing),
        reserved0: [0; 1],
        id: id_buf,
        name: name_buf,
        version,
        library_icon: icon_buf,
        purpose: [0; tairix_abi::BUNDLE_PURPOSE_MAX],
        author: [0; tairix_abi::BUNDLE_AUTHOR_MAX],
        syscall_table_hash: [0xAB; 32],
        content_hash: [0xCD; 32],
        signer_pubkey: [0xEF; 32],
        publisher_pubkey: [0xEF; 32],
        publisher_cert: [0; 64],
        signature: [0x99; 64],
    }
    .to_le_bytes()
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::LibraryCategory;

    /// A listed bundle is catalogued from **either** program store, at the
    /// path of the store it was planted in; an unlisted bundle and a
    /// service-store manifest never appear.
    #[test]
    fn listed_bundles_from_both_program_stores_are_catalogued() {
        let files = test_manifest(
            "files",
            Some(LibraryCategory::Accessories),
            Some("files.svg"),
        );
        let edit = test_manifest("edit", Some(LibraryCategory::Office), None);
        let ls = test_manifest("ls", None, None);
        let daemon = test_manifest("fontd", Some(LibraryCategory::Utilities), None);
        let run = vec![0u8; 4];
        let apps: Vec<(&[&[u8]], &[u8])> = vec![
            (
                &[b"Applications", b"files.app", b"AppInfo"],
                files.as_slice(),
            ),
            (&[b"Applications", b"files.app", b"Run"], run.as_slice()),
            (&[b"Commands", b"edit.app", b"AppInfo"], edit.as_slice()),
            (&[b"Commands", b"ls.app", b"AppInfo"], ls.as_slice()),
            // A Services-store manifest is never a library entry, whatever
            // it claims.
            (&[b"Services", b"fontd.app", b"AppInfo"], daemon.as_slice()),
            (&[b"Fonts", b"face.ttf"], run.as_slice()),
        ];

        let text = library_catalog(&apps).expect("derives");
        assert_eq!(
            text,
            "os.tairix.edit.name edit\n\
             os.tairix.edit.bundle /System/Commands/edit.app\n\
             os.tairix.edit.category Office\n\
             os.tairix.files.name files\n\
             os.tairix.files.bundle /System/Applications/files.app\n\
             os.tairix.files.category Accessories\n\
             os.tairix.files.icon files.svg\n"
        );
        // The derived document is a valid store the runtime readers accept.
        let catalog = tairix_proglib::parse(&text).expect("re-parses");
        assert_eq!(catalog.len(), 2);
    }

    #[test]
    fn no_listed_bundle_derives_the_canonical_empty_document() {
        let ls = test_manifest("ls", None, None);
        let apps: Vec<(&[&[u8]], &[u8])> =
            vec![(&[b"Commands", b"ls.app", b"AppInfo"], ls.as_slice())];
        assert_eq!(library_catalog(&apps).expect("derives"), "");
    }

    #[test]
    fn a_malformed_planted_manifest_fails_the_build_closed() {
        let apps: Vec<(&[&[u8]], &[u8])> =
            vec![(&[b"Commands", b"bad.app", b"AppInfo"], &[0u8; 8][..])];
        let err = library_catalog(&apps).expect_err("must fail closed");
        assert!(matches!(err, MkimageError::LibraryCatalog(_)));
    }

    #[test]
    fn two_bundles_claiming_one_identifier_fail_the_build_closed() {
        let first = test_manifest("edit", Some(LibraryCategory::Accessories), None);
        let apps: Vec<(&[&[u8]], &[u8])> = vec![
            (&[b"Commands", b"edit.app", b"AppInfo"], first.as_slice()),
            (&[b"Commands", b"edit2.app", b"AppInfo"], first.as_slice()),
        ];
        let err = library_catalog(&apps).expect_err("must fail closed");
        assert!(matches!(err, MkimageError::LibraryCatalog(_)));
    }
}
