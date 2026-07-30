//! Host tests for the `applib` engine: the grammar, every catalog
//! operation against in-memory seams, the fail-closed refusals, the
//! discovery-walk bounds, and the fd-3 advisory records.

extern crate std;

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::{
    Errno, LibraryCategory, ABI_VERSION_CURRENT, APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX,
    BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, SYSCALL_TABLE_HASH_LEN,
};
use tairix_help::HelpSource;

use super::{
    parse, push_json_string, run, AddRequest, AppLibError, Bundles, Command, DirEntryInfo, Output,
    Side, Store, Stores, MAX_WALK_DEPTH, MAX_WALK_ENTRIES, USAGE,
};

/// An in-memory store fixture: `None` models the absent document.
struct MemStore {
    text: RefCell<Option<String>>,
    writes: RefCell<usize>,
    read_err: Option<Errno>,
    write_err: Option<Errno>,
}

impl MemStore {
    fn new(text: Option<&str>) -> Self {
        Self {
            text: RefCell::new(text.map(String::from)),
            writes: RefCell::new(0),
            read_err: None,
            write_err: None,
        }
    }

    fn text(&self) -> Option<String> {
        self.text.borrow().clone()
    }

    fn writes(&self) -> usize {
        *self.writes.borrow()
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
        *self.writes.borrow_mut() += 1;
        *self.text.borrow_mut() = Some(text.to_string());
        Ok(())
    }
}

/// An in-memory store tree: directory listings plus per-bundle manifests.
#[derive(Default)]
struct MemBundles {
    dirs: BTreeMap<String, Vec<DirEntryInfo>>,
    manifests: BTreeMap<String, Result<Vec<u8>, Errno>>,
}

impl MemBundles {
    /// Record `path` as a directory containing `entries`
    /// (`(name, is_directory)` pairs).
    fn dir(&mut self, path: &str, entries: &[(&str, bool)]) {
        self.dirs.insert(
            path.to_owned(),
            entries
                .iter()
                .map(|(name, directory)| DirEntryInfo {
                    name: (*name).to_owned(),
                    directory: *directory,
                })
                .collect(),
        );
    }

    /// Record the manifest bytes served for the bundle at `path`.
    fn manifest(&mut self, path: &str, bytes: &[u8]) {
        self.manifests.insert(path.to_owned(), Ok(bytes.to_vec()));
    }

    /// Record a manifest read failure for the bundle at `path`.
    fn manifest_err(&mut self, path: &str, err: Errno) {
        self.manifests.insert(path.to_owned(), Err(err));
    }
}

impl Bundles for MemBundles {
    fn list_dir(&self, path: &str) -> Result<Option<Vec<DirEntryInfo>>, Errno> {
        Ok(self.dirs.get(path).cloned())
    }

    fn read_appinfo(&self, bundle: &str) -> Result<Option<Vec<u8>>, Errno> {
        match self.manifests.get(bundle) {
            Some(Ok(bytes)) => Ok(Some(bytes.clone())),
            Some(Err(err)) => Err(*err),
            None => Ok(None),
        }
    }
}

/// A capturing output fixture: stdout and the fd-3 records, separately.
#[derive(Default)]
struct MemOutput {
    out: RefCell<Vec<u8>>,
    info: RefCell<Vec<u8>>,
}

impl Output for MemOutput {
    fn out(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.out.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }

    fn info(&self, bytes: &[u8]) {
        self.info.borrow_mut().extend_from_slice(bytes);
    }
}

impl MemOutput {
    fn text(&self) -> String {
        String::from_utf8(self.out.borrow().clone()).expect("utf-8 output")
    }

    fn info_text(&self) -> String {
        String::from_utf8(self.info.borrow().clone()).expect("utf-8 records")
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

/// A decodable wire manifest for `name`, listed under `listing` with
/// `icon` when given.
fn manifest_bytes(name: &str, listing: Option<LibraryCategory>, icon: Option<&str>) -> Vec<u8> {
    fn inline<const N: usize>(text: &str) -> ([u8; N], u8) {
        let mut buf = [0u8; N];
        buf[..text.len()].copy_from_slice(text.as_bytes());
        (buf, u8::try_from(text.len()).expect("fits"))
    }
    let (id, id_len) = inline::<BUNDLE_ID_MAX>(&format!("os.tairix.{name}"));
    let (name_buf, name_len) = inline::<BUNDLE_NAME_MAX>(name);
    let (version, version_len) = inline::<BUNDLE_VERSION_MAX>("1.0");
    let (library_icon, library_icon_len) = inline::<LIBRARY_ICON_MAX>(icon.unwrap_or(""));
    tairix_abi::AppInfoHeader {
        magic: APPINFO_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: 0,
        capability_count: 0,
        mime_count: 0,
        id_len,
        name_len,
        version_len,
        library_icon_len,
        library: LibraryCategory::to_wire(listing),
        reserved0: [0; 3],
        id,
        name: name_buf,
        version,
        library_icon,
        syscall_table_hash: [0xAB; SYSCALL_TABLE_HASH_LEN],
        content_hash: [0xCD; 32],
        signer_pubkey: [0xEF; 32],
        signature: [0x99; 64],
    }
    .to_le_bytes()
    .to_vec()
}

/// A `Stores` view over two fixtures with a home.
fn stores<'a>(machine: &'a MemStore, user: &'a MemStore) -> Stores<'a> {
    Stores {
        machine,
        user: Some(user),
        home: Some("/Users/root"),
    }
}

/// A `Stores` view with no per-user overlay and no home.
fn homeless(machine: &MemStore) -> Stores<'_> {
    Stores {
        machine,
        user: None,
        home: None,
    }
}

#[test]
fn parse_maps_the_grammar() {
    assert_eq!(parse(&[]), Ok(Command::List { category: None }));
    assert_eq!(parse(&["list"]), Ok(Command::List { category: None }));
    assert_eq!(
        parse(&["list", "--category", "Games"]),
        Ok(Command::List {
            category: Some(LibraryCategory::Games)
        })
    );
    assert_eq!(
        parse(&[
            "add",
            "/Apps/chess.app",
            "--category=Games",
            "--name",
            "Chess",
            "--icon",
            "chess.svg",
            "--user"
        ]),
        Ok(Command::Add(AddRequest {
            bundle: "/Apps/chess.app",
            category: Some(LibraryCategory::Games),
            name: Some("Chess"),
            icon: Some("chess.svg"),
            user: true,
        }))
    );
    assert_eq!(
        parse(&["remove", "os.tairix.chess"]),
        Ok(Command::Remove {
            target: "os.tairix.chess",
            user: false
        })
    );
    assert_eq!(
        parse(&["hide", "os.tairix.chess", "--user"]),
        Ok(Command::Hide {
            id: "os.tairix.chess",
            user: true
        })
    );
    assert_eq!(
        parse(&["show", "os.tairix.chess"]),
        Ok(Command::Show {
            id: "os.tairix.chess",
            user: false
        })
    );
    assert_eq!(
        parse(&["rescan", "--user"]),
        Ok(Command::Rescan { user: true })
    );
    // The reserved short-help switches win wherever they appear.
    for line in [
        &["-h"][..],
        &["--help"][..],
        &["add", "-?"][..],
        &["rescan", "--help"][..],
    ] {
        assert_eq!(parse(line), Ok(Command::Help), "{line:?}");
    }
    // `--` ends option parsing, admitting an operand spelled like an option.
    assert_eq!(
        parse(&["remove", "--", "--user"]),
        Ok(Command::Remove {
            target: "--user",
            user: false
        })
    );
}

#[test]
fn parse_refuses_what_the_grammar_does_not_define() {
    for (line, expected) in [
        (&["frobnicate"][..], AppLibError::Usage),
        (&["list", "extra"][..], AppLibError::Usage),
        (&["list", "--category"][..], AppLibError::Usage),
        (
            &["list", "--category", "Stuff"][..],
            AppLibError::UnknownFolder,
        ),
        (
            &["list", "--category", "games"][..],
            AppLibError::UnknownFolder,
        ),
        (&["add"][..], AppLibError::Usage),
        (
            &["add", "/Apps/a.app", "/Apps/b.app"][..],
            AppLibError::Usage,
        ),
        (&["add", "/Apps/a.app", "--frob"][..], AppLibError::Usage),
        (&["add", "/Apps/a.app", "-x"][..], AppLibError::Usage),
        (
            &["add", "/Apps/a.app", "--user=yes"][..],
            AppLibError::Usage,
        ),
        (&["remove"][..], AppLibError::Usage),
        (&["hide"][..], AppLibError::Usage),
        (&["rescan", "operand"][..], AppLibError::Usage),
    ] {
        assert_eq!(parse(line), Err(expected), "{line:?}");
    }
}

#[test]
fn list_shows_the_resolved_library_folder_by_folder() {
    let machine = MemStore::new(Some(
        "os.tairix.edit.name Edit\n\
         os.tairix.edit.bundle /System/Apps/edit.app\n\
         os.tairix.edit.category Accessories\n\
         os.tairix.chess.name Chess\n\
         os.tairix.chess.bundle /Apps/chess.app\n\
         os.tairix.chess.category Games\n",
    ));
    // The user's overlay renames Edit; the resolved view shows the rename.
    let user = MemStore::new(Some("os.tairix.edit.name My Editor\n"));
    let output = MemOutput::default();
    run(
        Command::List { category: None },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("lists");
    assert_eq!(
        output.text(),
        "Accessories\n  os.tairix.edit  My Editor  /System/Apps/edit.app\n\
         Games\n  os.tairix.chess  Chess  /Apps/chess.app\n"
    );

    // A folder filter shows only that folder.
    let filtered = MemOutput::default();
    run(
        Command::List {
            category: Some(LibraryCategory::Games),
        },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &filtered,
    )
    .expect("lists");
    assert_eq!(
        filtered.text(),
        "Games\n  os.tairix.chess  Chess  /Apps/chess.app\n"
    );
}

#[test]
fn list_without_stores_prints_nothing() {
    let machine = MemStore::new(None);
    let output = MemOutput::default();
    run(
        Command::List { category: None },
        None,
        &homeless(&machine),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("lists");
    assert_eq!(output.text(), "");
}

#[test]
fn add_derives_the_entry_from_the_bundles_manifest() {
    let machine = MemStore::new(None);
    let user = MemStore::new(None);
    let mut bundles = MemBundles::default();
    bundles.manifest(
        "/Apps/chess.app",
        &manifest_bytes("chess", Some(LibraryCategory::Games), Some("chess.svg")),
    );
    let output = MemOutput::default();
    run(
        Command::Add(AddRequest {
            bundle: "/Apps/chess.app/",
            category: None,
            name: None,
            icon: None,
            user: false,
        }),
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &output,
    )
    .expect("adds");
    assert_eq!(
        machine.text().as_deref(),
        Some(
            "os.tairix.chess.name chess\n\
             os.tairix.chess.bundle /Apps/chess.app\n\
             os.tairix.chess.category Games\n\
             os.tairix.chess.icon chess.svg\n"
        )
    );
    // Quiet on stdout; the outcome is an fd-3 summary record.
    assert_eq!(output.text(), "");
    let info = output.info_text();
    assert!(
        info.contains("\"code\":\"apps.library_entry_added\""),
        "{info}"
    );
    assert!(info.contains("Registered chess under Games."), "{info}");
    assert!(info.ends_with('\n'), "framed JSONL: {info}");
}

#[test]
fn add_overrides_win_over_the_manifest() {
    let machine = MemStore::new(None);
    let user = MemStore::new(None);
    let mut bundles = MemBundles::default();
    bundles.manifest(
        "/Apps/chess.app",
        &manifest_bytes("chess", Some(LibraryCategory::Games), None),
    );
    let output = MemOutput::default();
    run(
        Command::Add(AddRequest {
            bundle: "/Apps/chess.app",
            category: Some(LibraryCategory::Utilities),
            name: Some("Grandmaster"),
            icon: Some("gm.svg"),
            user: false,
        }),
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &output,
    )
    .expect("adds");
    let text = machine.text().expect("written");
    assert!(
        text.contains("os.tairix.chess.name Grandmaster\n"),
        "{text}"
    );
    assert!(
        text.contains("os.tairix.chess.category Utilities\n"),
        "{text}"
    );
    assert!(text.contains("os.tairix.chess.icon gm.svg\n"), "{text}");
}

#[test]
fn add_refuses_what_it_cannot_derive_and_changes_nothing() {
    let machine = MemStore::new(None);
    let user = MemStore::new(None);
    let mut bundles = MemBundles::default();
    bundles.manifest("/Apps/tool.app", &manifest_bytes("tool", None, None));
    bundles.manifest("/Apps/bad.app", &[0u8; 16]);
    bundles.manifest_err("/Apps/locked.app", Errno::PermissionDenied);
    let output = MemOutput::default();
    let add = |bundle, category| {
        run(
            Command::Add(AddRequest {
                bundle,
                category,
                name: None,
                icon: None,
                user: false,
            }),
            None,
            &stores(&machine, &user),
            &bundles,
            &NoHelp,
            &output,
        )
    };

    // An unlisted manifest needs an explicit folder.
    assert_eq!(add("/Apps/tool.app", None), Err(AppLibError::NotListed));
    // …and with one, it registers.
    add("/Apps/tool.app", Some(LibraryCategory::Utilities)).expect("adds");
    assert!(machine
        .text()
        .expect("written")
        .contains("category Utilities"));

    assert_eq!(add("/Apps/ghost.app", None), Err(AppLibError::NoManifest));
    assert_eq!(add("/Apps/bad.app", None), Err(AppLibError::BadManifest));
    assert_eq!(
        add("/Apps/locked.app", None),
        Err(AppLibError::Bundle(Errno::PermissionDenied))
    );
    // A bundle path outside every application store is refused by the model.
    assert!(matches!(
        add("/Storage/usb0/x.app", None),
        Err(AppLibError::Entry(_) | AppLibError::NoManifest)
    ));
}

#[test]
fn a_refused_machine_write_surfaces_and_changes_nothing() {
    let mut machine = MemStore::new(Some(
        "os.tairix.edit.name Edit\nos.tairix.edit.bundle /Apps/edit.app\n",
    ));
    machine.write_err = Some(Errno::PermissionDenied);
    let user = MemStore::new(None);
    let mut bundles = MemBundles::default();
    bundles.manifest(
        "/Apps/chess.app",
        &manifest_bytes("chess", Some(LibraryCategory::Games), None),
    );
    let output = MemOutput::default();
    let err = run(
        Command::Add(AddRequest {
            bundle: "/Apps/chess.app",
            category: None,
            name: None,
            icon: None,
            user: false,
        }),
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &output,
    )
    .expect_err("refused");
    assert_eq!(
        err,
        AppLibError::Write(Side::Machine, Errno::PermissionDenied)
    );
    assert_eq!(
        format!("{err}"),
        "cannot write the machine store: permission denied"
    );
    // No advisory record for a change that did not happen.
    assert_eq!(output.info_text(), "");
}

#[test]
fn user_operations_without_a_home_fail_closed() {
    let machine = MemStore::new(None);
    let output = MemOutput::default();
    for command in [
        Command::Add(AddRequest {
            bundle: "/Apps/chess.app",
            category: None,
            name: None,
            icon: None,
            user: true,
        }),
        Command::Remove {
            target: "os.tairix.chess",
            user: true,
        },
        Command::Rescan { user: true },
    ] {
        assert_eq!(
            run(
                command,
                None,
                &homeless(&machine),
                &MemBundles::default(),
                &NoHelp,
                &output
            ),
            Err(AppLibError::NoHome)
        );
    }
    assert_eq!(machine.writes(), 0);
}

#[test]
fn remove_takes_an_identifier_or_a_bundle_path() {
    let text = "os.tairix.chess.name Chess\nos.tairix.chess.bundle /Apps/chess.app\n";
    let machine = MemStore::new(Some(text));
    let user = MemStore::new(None);
    let output = MemOutput::default();
    run(
        Command::Remove {
            target: "os.tairix.chess",
            user: false,
        },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("removes");
    assert_eq!(machine.text().as_deref(), Some(""));
    let info = output.info_text();
    assert!(
        info.contains("\"code\":\"apps.library_entry_removed\""),
        "{info}"
    );

    let machine = MemStore::new(Some(text));
    run(
        Command::Remove {
            target: "/Apps/chess.app/",
            user: false,
        },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("removes by bundle path");
    assert_eq!(machine.text().as_deref(), Some(""));

    let machine = MemStore::new(Some(text));
    assert_eq!(
        run(
            Command::Remove {
                target: "os.tairix.ghost",
                user: false,
            },
            None,
            &stores(&machine, &user),
            &MemBundles::default(),
            &NoHelp,
            &output,
        ),
        Err(AppLibError::UnknownEntry)
    );
    assert_eq!(machine.writes(), 0, "a refused remove changes nothing");
}

#[test]
fn hide_and_show_record_the_visibility_verdict() {
    let machine = MemStore::new(Some(
        "os.tairix.chess.name Chess\nos.tairix.chess.bundle /Apps/chess.app\n",
    ));
    let user = MemStore::new(None);
    let output = MemOutput::default();

    // A machine-side hide flips the declared entry's own flag.
    run(
        Command::Hide {
            id: "os.tairix.chess",
            user: false,
        },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("hides");
    assert!(
        machine
            .text()
            .expect("written")
            .contains("os.tairix.chess.hidden true\n"),
        "{:?}",
        machine.text()
    );

    // The resolved listing no longer shows it…
    let listing = MemOutput::default();
    run(
        Command::List { category: None },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &listing,
    )
    .expect("lists");
    assert_eq!(listing.text(), "");

    // …until the user's own overlay re-shows it (the overlay verdict wins).
    run(
        Command::Show {
            id: "os.tairix.chess",
            user: true,
        },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("shows");
    assert_eq!(
        user.text().as_deref(),
        Some("os.tairix.chess.hidden false\n"),
        "the overlay records a patch, not a copy of the entry"
    );
    let listing = MemOutput::default();
    run(
        Command::List { category: None },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &listing,
    )
    .expect("lists");
    assert!(
        listing.text().contains("os.tairix.chess"),
        "{}",
        listing.text()
    );

    // A verdict for an identifier no record claims is refused.
    assert_eq!(
        run(
            Command::Hide {
                id: "os.tairix.ghost",
                user: false,
            },
            None,
            &stores(&machine, &user),
            &MemBundles::default(),
            &NoHelp,
            &output,
        ),
        Err(AppLibError::UnknownEntry)
    );
}

#[test]
fn rescan_registers_only_listed_bundles_and_never_disturbs_curation() {
    // The machine store already curates chess (renamed) and suppresses
    // solitaire; a rescan must disturb neither.
    let machine = MemStore::new(Some(
        "os.tairix.chess.name Grandmaster\n\
         os.tairix.chess.bundle /Apps/chess.app\n\
         os.tairix.chess.category Games\n\
         os.tairix.solitaire.name Solitaire\n\
         os.tairix.solitaire.bundle /Apps/solitaire.app\n\
         os.tairix.solitaire.category Games\n\
         os.tairix.solitaire.hidden true\n",
    ));
    let user = MemStore::new(None);
    let mut bundles = MemBundles::default();
    bundles.dir(
        "/System/Apps",
        &[("edit.app", true), ("ls.app", true), ("notes.txt", false)],
    );
    bundles.dir(
        "/Apps",
        &[
            ("chess.app", true),
            ("solitaire.app", true),
            ("bad.app", true),
            ("games", true),
        ],
    );
    // Nested plain subdirectories are walked; bundles are sealed units.
    bundles.dir("/Apps/games", &[("mahjong.app", true)]);
    bundles.manifest(
        "/System/Apps/edit.app",
        &manifest_bytes("edit", Some(LibraryCategory::Accessories), None),
    );
    // A command tool with no listing is not a library application.
    bundles.manifest("/System/Apps/ls.app", &manifest_bytes("ls", None, None));
    bundles.manifest(
        "/Apps/chess.app",
        &manifest_bytes("chess", Some(LibraryCategory::Games), None),
    );
    bundles.manifest(
        "/Apps/solitaire.app",
        &manifest_bytes("solitaire", Some(LibraryCategory::Games), None),
    );
    bundles.manifest("/Apps/bad.app", &[0u8; 16]);
    bundles.manifest(
        "/Apps/games/mahjong.app",
        &manifest_bytes("mahjong", Some(LibraryCategory::Games), None),
    );

    let output = MemOutput::default();
    run(
        Command::Rescan { user: false },
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &output,
    )
    .expect("rescans");

    let text = machine.text().expect("written");
    // The new bundles are registered under their declared folders…
    assert!(
        text.contains("os.tairix.edit.bundle /System/Apps/edit.app\n"),
        "{text}"
    );
    assert!(
        text.contains("os.tairix.mahjong.bundle /Apps/games/mahjong.app\n"),
        "{text}"
    );
    // …the unlisted tool is not…
    assert!(!text.contains("os.tairix.ls"), "{text}");
    // …and the curated rename and suppression stand untouched.
    assert!(
        text.contains("os.tairix.chess.name Grandmaster\n"),
        "{text}"
    );
    assert!(text.contains("os.tairix.solitaire.hidden true\n"), "{text}");

    let info = output.info_text();
    assert!(info.contains("\"code\":\"apps.library_rescan\""), "{info}");
    assert!(
        info.contains("Registered 2 new application(s); skipped 1."),
        "{info}"
    );

    // A second rescan finds nothing new and does not rewrite the store.
    let writes = machine.writes();
    run(
        Command::Rescan { user: false },
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &MemOutput::default(),
    )
    .expect("rescans idempotently");
    assert_eq!(
        machine.writes(),
        writes,
        "an unchanged catalog is not rewritten"
    );
}

#[test]
fn rescan_user_walks_the_home_store_into_the_overlay() {
    let machine = MemStore::new(None);
    let user = MemStore::new(None);
    let mut bundles = MemBundles::default();
    bundles.dir("/Users/root/Apps", &[("paint.app", true)]);
    bundles.manifest(
        "/Users/root/Apps/paint.app",
        &manifest_bytes("paint", Some(LibraryCategory::Graphics), None),
    );
    let output = MemOutput::default();
    run(
        Command::Rescan { user: true },
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &output,
    )
    .expect("rescans");
    assert_eq!(machine.writes(), 0, "the machine store is untouched");
    assert!(
        user.text()
            .expect("written")
            .contains("os.tairix.paint.bundle /Users/root/Apps/paint.app\n"),
        "{:?}",
        user.text()
    );
}

#[test]
fn the_rescan_walk_is_bounded() {
    let machine = MemStore::new(None);
    let user = MemStore::new(None);

    // Depth: a bundle nested beyond the walk bound is never reached.
    let mut bundles = MemBundles::default();
    let mut dir = String::from("/Apps");
    for level in 0..MAX_WALK_DEPTH {
        let child = format!("{dir}/d{level}");
        bundles.dir(&dir, &[(&format!("d{level}"), true)]);
        dir = child;
    }
    bundles.dir(&dir, &[("deep.app", true)]);
    bundles.manifest(
        &format!("{dir}/deep.app"),
        &manifest_bytes("deep", Some(LibraryCategory::Games), None),
    );
    run(
        Command::Rescan { user: false },
        None,
        &stores(&machine, &user),
        &bundles,
        &NoHelp,
        &MemOutput::default(),
    )
    .expect("rescans");
    assert_eq!(machine.writes(), 0, "nothing within bounds to register");

    // Entries: a tree wider than the bound fails the scan closed.
    let mut bundles = MemBundles::default();
    let wide: Vec<String> = (0..=MAX_WALK_ENTRIES).map(|i| format!("f{i}")).collect();
    let entries: Vec<(&str, bool)> = wide.iter().map(|name| (name.as_str(), false)).collect();
    bundles.dir("/System/Apps", &entries);
    assert_eq!(
        run(
            Command::Rescan { user: false },
            None,
            &stores(&machine, &user),
            &bundles,
            &NoHelp,
            &MemOutput::default(),
        ),
        Err(AppLibError::TreeTooLarge)
    );
    assert_eq!(machine.writes(), 0);
}

#[test]
fn a_malformed_store_refuses_every_operation_naming_the_side() {
    let machine = MemStore::new(Some("os.tairix.chess.frob what\n"));
    let user = MemStore::new(None);
    let output = MemOutput::default();
    let err = run(
        Command::List { category: None },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect_err("refused");
    assert!(matches!(err, AppLibError::Malformed(Side::Machine, _)));
    assert!(format!("{err}").starts_with("the machine store is not understood:"));

    let machine = MemStore::new(None);
    let user = MemStore::new(Some("os.tairix.chess.frob what\n"));
    let err = run(
        Command::List { category: None },
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect_err("refused");
    assert!(matches!(err, AppLibError::Malformed(Side::User, _)));
}

#[test]
fn store_errors_surface_with_their_errno_and_side() {
    let mut machine = MemStore::new(None);
    machine.read_err = Some(Errno::PermissionDenied);
    let user = MemStore::new(None);
    let output = MemOutput::default();
    assert_eq!(
        run(
            Command::List { category: None },
            None,
            &stores(&machine, &user),
            &MemBundles::default(),
            &NoHelp,
            &output
        ),
        Err(AppLibError::Read(Side::Machine, Errno::PermissionDenied))
    );
}

#[test]
fn help_falls_back_to_the_usage_banner_without_documents() {
    let machine = MemStore::new(None);
    let user = MemStore::new(None);
    let output = MemOutput::default();
    run(
        Command::Help,
        None,
        &stores(&machine, &user),
        &MemBundles::default(),
        &NoHelp,
        &output,
    )
    .expect("help renders");
    assert_eq!(output.text(), format!("{USAGE}\n"));
}

#[test]
fn json_strings_escape_what_could_break_the_record() {
    let mut out = String::new();
    push_json_string(&mut out, "plain");
    assert_eq!(out, "\"plain\"");

    let mut out = String::new();
    push_json_string(&mut out, "a\"b\\c\nd");
    assert_eq!(out, "\"a\\\"b\\\\c\\u000ad\"");
}

/// Every locale's Help document names the subcommands, the switches, and
/// every folder of the closed taxonomy (`plans/APPS.md`): the tokens are
/// language-neutral, so each translated document must carry the same ones
/// as the canonical `en-US`. The documents are read from the bundle's own
/// on-disk `Help/` tree — the single source the image builder plants —
/// never a copy embedded in this crate.
#[test]
fn help_documents_the_subcommands_switches_and_folders() {
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = format!("{help_root}/{locale}/applib.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for token in [
            "`applib add`",
            "`applib remove`",
            "`applib hide`",
            "`applib show`",
            "`applib rescan`",
            "`--category`",
            "`--name`",
            "`--icon`",
            "`--user`",
            "`-h, -?`",
        ] {
            assert!(
                text.contains(token),
                "{locale}/applib.md must document {token}"
            );
        }
        for folder in LibraryCategory::ALL {
            assert!(
                text.contains(folder.as_str()),
                "{locale}/applib.md must document the {folder} folder"
            );
        }
    }
}
