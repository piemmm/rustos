//! The `unmount` engine: resolve the named volume through the System
//! Information API's mount listing, then hand its identity to the
//! kernel's detach path.

use alloc::format;
use alloc::string::String;

use rustos_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use rustos_abi::sysinfo::{MountAvailability, MOUNT_VOLUME_ID_LEN};
use rustos_help::{own_short_help, HelpSource};
use rustos_procinfo::{for_each_mount, Transport};

use crate::command::Command;
use crate::error::UnmountError;
use crate::io::{Detacher, Output};

/// The one-line usage banner, printed on a usage error and as the
/// fallback when the bundled help document is unavailable.
pub const USAGE: &str = "usage: unmount [-f | --force] [--] NAME";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "unmount";

/// The facts the resolver needs about the matched mount: the volume's
/// stable identity and whether the listing already marks it unavailable.
struct Resolved {
    volume_id: [u8; MOUNT_VOLUME_ID_LEN],
    unavailable: bool,
}

/// Run a parsed `unmount` command against the injected seams.
///
/// A successful detach writes nothing, matching the established `umount`
/// behaviour; the kernel logs the audited decision.
///
/// # Errors
///
/// * [`UnmountError::NotFound`] — no mounted filesystem matches `NAME`.
/// * [`UnmountError::NotDetachable`] — the matched mount carries no
///   detachable volume identity (a permanent boot volume or view
///   binding).
/// * [`UnmountError::Detach`] — the kernel refused or failed the detach;
///   `unavailable` distinguishes the surprise-removed case whose plain
///   detach is refused by design (the `--force` suggestion is also
///   emitted on fd 3, additive and ignorable).
/// * [`UnmountError::Service`] — the mount-table query failed.
/// * [`UnmountError::Output`] — the short help could not be written.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    detacher: &dyn Detacher,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(), UnmountError> {
    let (name, force) = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(UnmountError::Output)?;
            return Ok(());
        }
        Command::Unmount { name, force } => (name, force),
    };

    let resolved =
        resolve(&name, transport)?.ok_or_else(|| UnmountError::NotFound(name.clone()))?;
    if resolved.volume_id == [0u8; MOUNT_VOLUME_ID_LEN] {
        return Err(UnmountError::NotDetachable(name));
    }
    match detacher.detach(resolved.volume_id, force) {
        Ok(()) => Ok(()),
        Err(errno) => {
            if resolved.unavailable && !force {
                emit_force_suggestion(err, &name);
            }
            Err(UnmountError::Detach {
                errno,
                unavailable: resolved.unavailable,
            })
        }
    }
}

/// Resolve `name` against the mount listing: the first record whose
/// backing source, mount-point path, or `/Storage/<name>` catalog
/// location matches. Listing order is the service's stable mount order,
/// and catalog names are unique in the mount table, so the first match
/// is the only match for a detachable volume.
fn resolve(name: &str, transport: &dyn Transport) -> Result<Option<Resolved>, UnmountError> {
    let catalog_path = format!("/Storage/{name}");
    let mut found: Option<Resolved> = None;
    for_each_mount(transport, |record| {
        if found.is_some() {
            return Ok(());
        }
        let source = String::from_utf8_lossy(record.source_bytes());
        let target = String::from_utf8_lossy(record.target_bytes());
        if source == name || target == name || target == catalog_path {
            found = Some(Resolved {
                volume_id: record.volume_id(),
                unavailable: record.availability() != MountAvailability::Available,
            });
        }
        Ok(())
    })
    .map_err(UnmountError::from)?;
    Ok(found)
}

/// Emit the fd-3 `suggestion` record for a refused plain detach of an
/// unavailable volume: the retained data is held deliberately, and the
/// audited force-unmount is the explicit way to give it up. Best-effort
/// and additive — it never changes the diagnostic, exit status, or
/// pipeline behaviour.
fn emit_force_suggestion(err: &dyn Output, name: &str) {
    let message = String::from("The volume holds retained uncommitted data.");
    let suggestion = format!("Use `unmount --force {name}` to discard it.");
    let ai = format!(
        "{{\"subject\":\"volume_detach\",\
         \"refusal\":{{\"reason\":\"volume_unavailable\",\
         \"retained_data_would_be_discarded\":true}},\
         \"suggestion\":{{\"argv\":[\"unmount\",\"--force\",\"{name}\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Suggestion,
        "fs.volume_unavailable_force_required",
        Severity::Info,
        Human::with_suggestion(&message, &suggestion),
    )
    .with_ai(&ai);
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        err.info(&buf[..len]);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::UnmountError;
    use crate::io::{Detacher, Output};
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::driver::filesystem::{MountFlags, VolumeStats};
    use rustos_abi::sysinfo::{
        MountAvailability, MountListRequest, MountRecord, SysinfoRequestHeader,
    };
    use rustos_abi::Errno;
    use rustos_help::{HelpSource, SourceError};
    use rustos_procinfo::Transport;

    /// An in-memory `sysinfod` stand-in answering mount-list queries from
    /// a fixture, decoding the request the same way the real service does.
    struct Fixture {
        records: Vec<MountRecord>,
        fail: Option<Errno>,
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            if let Some(errno) = self.fail {
                return Err(errno);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = MountListRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * MountRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    /// One recorded detach request.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Detached {
        volume_id: [u8; 16],
        force: bool,
    }

    /// Records every detach request, optionally failing it.
    struct MemDetacher {
        fail: Option<Errno>,
        detached: RefCell<Vec<Detached>>,
    }

    impl MemDetacher {
        fn new() -> Self {
            Self {
                fail: None,
                detached: RefCell::new(Vec::new()),
            }
        }
        fn failing(errno: Errno) -> Self {
            Self {
                fail: Some(errno),
                detached: RefCell::new(Vec::new()),
            }
        }
    }

    impl Detacher for MemDetacher {
        fn detach(&self, volume_id: [u8; 16], force: bool) -> Result<(), Errno> {
            self.detached
                .borrow_mut()
                .push(Detached { volume_id, force });
            match self.fail {
                Some(errno) => Err(errno),
                None => Ok(()),
            }
        }
    }

    /// Captures written bytes and fd-3 records.
    struct Recorder {
        written: RefCell<Vec<u8>>,
        infos: RefCell<Vec<String>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                written: RefCell::new(Vec::new()),
                infos: RefCell::new(Vec::new()),
            }
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.written.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
        fn info(&self, record: &[u8]) {
            self.infos
                .borrow_mut()
                .push(String::from_utf8_lossy(record).into_owned());
        }
    }

    /// A help tree with no documents, so the usage banner is the fallback.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    fn record(
        source: &[u8],
        target: &[u8],
        availability: MountAvailability,
        volume_id: [u8; 16],
    ) -> MountRecord {
        MountRecord::new(
            source,
            target,
            b"fat32",
            MountFlags::NOSUID,
            VolumeStats::default(),
            availability,
            volume_id,
        )
        .expect("record")
    }

    fn table() -> Fixture {
        Fixture {
            records: alloc::vec![
                record(b"ARXFSRoot", b"/", MountAvailability::Available, [0u8; 16]),
                record(
                    b"usb1",
                    b"/Storage/usb1",
                    MountAvailability::Available,
                    [1u8; 16],
                ),
                record(
                    b"usb2",
                    b"/Storage/usb2",
                    MountAvailability::UnavailableDirty,
                    [2u8; 16],
                ),
            ],
            fail: None,
        }
    }

    fn run_args(
        args: &[&str],
        fixture: &Fixture,
        detacher: &MemDetacher,
        err: &Recorder,
    ) -> Result<(), UnmountError> {
        let out = Recorder::new();
        run(
            parse(args).expect("valid command"),
            None,
            fixture,
            detacher,
            &NoHelp,
            &out,
            err,
        )
    }

    #[test]
    fn help_prints_the_usage_fallback_and_detaches_nothing() {
        let detacher = MemDetacher::new();
        let out = Recorder::new();
        let err = Recorder::new();
        run(
            parse(&["--help"]).expect("valid"),
            None,
            &table(),
            &detacher,
            &NoHelp,
            &out,
            &err,
        )
        .expect("help succeeds");
        assert!(String::from_utf8_lossy(&out.written.borrow()).contains(USAGE));
        assert!(detacher.detached.borrow().is_empty());
    }

    #[test]
    fn a_catalog_name_resolves_to_its_volume_identity() {
        let detacher = MemDetacher::new();
        let err = Recorder::new();
        run_args(&["usb1"], &table(), &detacher, &err).expect("detaches");
        assert_eq!(
            detacher.detached.borrow().as_slice(),
            &[Detached {
                volume_id: [1u8; 16],
                force: false,
            }]
        );
    }

    #[test]
    fn a_mount_point_path_resolves_too() {
        let detacher = MemDetacher::new();
        let err = Recorder::new();
        run_args(&["/Storage/usb1"], &table(), &detacher, &err).expect("detaches");
        assert_eq!(detacher.detached.borrow()[0].volume_id, [1u8; 16]);
    }

    #[test]
    fn force_reaches_the_kernel() {
        let detacher = MemDetacher::new();
        let err = Recorder::new();
        run_args(&["-f", "usb2"], &table(), &detacher, &err).expect("force detaches");
        assert_eq!(
            detacher.detached.borrow().as_slice(),
            &[Detached {
                volume_id: [2u8; 16],
                force: true,
            }]
        );
        // A force request carries no refusal, so no advisory is emitted.
        assert!(err.infos.borrow().is_empty());
    }

    #[test]
    fn an_unknown_name_is_not_found_and_detaches_nothing() {
        let detacher = MemDetacher::new();
        let err = Recorder::new();
        assert_eq!(
            run_args(&["usb9"], &table(), &detacher, &err),
            Err(UnmountError::NotFound(String::from("usb9")))
        );
        assert!(detacher.detached.borrow().is_empty());
    }

    #[test]
    fn a_volume_without_identity_is_not_detachable() {
        // The boot root has no published detachable identity: refusing
        // locally beats sending the kernel a nil identity it must refuse.
        let detacher = MemDetacher::new();
        let err = Recorder::new();
        assert_eq!(
            run_args(&["/"], &table(), &detacher, &err),
            Err(UnmountError::NotDetachable(String::from("/")))
        );
        assert!(detacher.detached.borrow().is_empty());
    }

    #[test]
    fn a_refused_plain_detach_of_an_unavailable_volume_suggests_force() {
        let detacher = MemDetacher::failing(Errno::DeviceFault);
        let err = Recorder::new();
        assert_eq!(
            run_args(&["usb2"], &table(), &detacher, &err),
            Err(UnmountError::Detach {
                errno: Errno::DeviceFault,
                unavailable: true,
            })
        );
        let infos = err.infos.borrow();
        assert_eq!(infos.len(), 1);
        assert!(infos[0].contains("\"suggestion\""));
        assert!(infos[0].contains("--force"));
        assert!(infos[0].contains("usb2"));
    }

    #[test]
    fn a_refused_detach_of_a_healthy_volume_emits_no_advisory() {
        let detacher = MemDetacher::failing(Errno::PermissionDenied);
        let err = Recorder::new();
        assert_eq!(
            run_args(&["usb1"], &table(), &detacher, &err),
            Err(UnmountError::Detach {
                errno: Errno::PermissionDenied,
                unavailable: false,
            })
        );
        assert!(err.infos.borrow().is_empty());
    }

    /// Every locale's `OPTIONS` section documents exactly the switches
    /// this parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the
    /// bundle's own on-disk `Help/` tree — the single source the image
    /// builder plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::fs;

        let help_root = alloc::format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        for locale in rustos_help::REQUIRED_LOCALES {
            let path = alloc::format!("{help_root}/{locale}/unmount.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-f, --force`", "`-?, --help`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/unmount.md must document {switch}"
                );
            }
        }
    }

    #[test]
    fn a_listing_failure_surfaces_as_service() {
        let mut fixture = table();
        fixture.fail = Some(Errno::NotFound);
        let detacher = MemDetacher::new();
        let err = Recorder::new();
        assert_eq!(
            run_args(&["usb1"], &fixture, &detacher, &err),
            Err(UnmountError::Service(Errno::NotFound))
        );
        assert!(detacher.detached.borrow().is_empty());
    }
}
