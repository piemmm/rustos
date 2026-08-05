//! The shared mount-list paging walk and row rendering.
//!
//! The `mount` tool pages through the system mount table and renders each
//! [`MountRecord`] into a single line in the familiar
//! `source on target type fstype (options)` shape. The paging is the generic
//! [`walk_pages`](crate::list) used by the process list, so only the
//! per-record decode and the row rendering live here.

use alloc::format;
use alloc::string::String;

use tairix_abi::driver::filesystem::MountFlags;
use tairix_abi::sysinfo::{MountAvailability, MountListRequest, MountRecord, SysinfoQueryId};
use tairix_abi::Errno;

use crate::list::{field_lossy, walk_pages, ListError, WalkStep};
use crate::request::CallError;
use crate::transport::Transport;

/// Number of [`MountRecord`]s requested per mount-list page.
///
/// A page bounds the reply size so the transport never has to carry the
/// whole mount table at once; [`for_each_mount`] walks pages until a short
/// page ends the list.
pub const MOUNT_PAGE: u16 = 64;

/// Page through the mount table and hand each decoded [`MountRecord`] to
/// `sink`.
///
/// The query is [`SysinfoQueryId::MOUNT_LIST`], which the service serves
/// ungated: the mount table is system-wide and secret-free. Records are delivered in the order the service returns them.
///
/// `sink` answers [`WalkStep::Continue`] to be given the next record or
/// [`WalkStep::Stop`] to end the walk there, which is how a caller bounds
/// how much of a long or hostile list it will accept. Stopping is an
/// ordinary success, so it stays distinguishable from a failure.
///
/// The walk **fails closed**: a reply whose length
/// is not a whole number of [`MountRecord::WIRE_LEN`] records, or one that
/// would overflow the page offset, is rejected rather than partially
/// rendered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_mount(
    transport: &dyn Transport,
    mut sink: impl FnMut(&MountRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::MOUNT_LIST,
        MountRecord::WIRE_LEN,
        MOUNT_PAGE,
        |offset, limit| {
            MountListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = MountRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Render one [`MountRecord`] as a `source on target type fstype (options)`
/// line, the long-standing Unix `mount` listing shape.
///
/// A surprise-removed volume carries a trailing ` [unavailable-dirty]` /
/// ` [unavailable-lost]` marker, a re-inserted volume whose
/// non-mutation could not be proven carries ` [recovery-conflict]`
/// (`plans/DEVICES.md` D4c), and a live-but-unwell device carries
/// ` [degraded]` or ` [recovering]` from its reported I/O health
/// (`plans/FIX-IO.md` IO3) — additive, so a healthy volume's line is
/// byte-identical to the classic shape while an unwell, dead, or
/// conflicted one never looks healthy (`plans/DEVICES.md` D4b).
#[must_use]
pub fn render_mount(record: &MountRecord) -> String {
    let marker = match record.availability() {
        MountAvailability::Available => "",
        MountAvailability::UnavailableDirty => " [unavailable-dirty]",
        MountAvailability::UnavailableLost => " [unavailable-lost]",
        MountAvailability::RecoveryConflict => " [recovery-conflict]",
        MountAvailability::Degraded => " [degraded]",
        MountAvailability::Recovering => " [recovering]",
    };
    format!(
        "{} on {} type {} ({}){}",
        field_lossy(record.source_bytes()),
        field_lossy(record.target_bytes()),
        field_lossy(record.fstype_bytes()),
        render_options(record.flags()),
        marker,
    )
}

/// Render the mount-policy flags as a comma-separated option list.
///
/// The list always opens with `ro` or `rw`, then appends each restriction in
/// force, matching how a Unix `mount` listing reads.
#[must_use]
pub fn render_options(flags: MountFlags) -> String {
    let mut options = String::from(if flags.contains(MountFlags::READ_ONLY) {
        "ro"
    } else {
        "rw"
    });
    for (flag, name) in [
        (MountFlags::NOSUID, "nosuid"),
        (MountFlags::NODEV, "nodev"),
        (MountFlags::NOEXEC, "noexec"),
    ] {
        if flags.contains(flag) {
            options.push(',');
            options.push_str(name);
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::{for_each_mount, render_mount, render_options, WalkStep, MOUNT_PAGE};
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::blkio::BlkDeviceClass;
    use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
    use tairix_abi::sysinfo::{
        MountAvailability, MountListRequest, MountRecord, MountVolumeState, SysinfoQueryId,
        SysinfoRequestHeader,
    };
    use tairix_abi::Errno;

    /// An in-memory `sysinfod` stand-in answering mount-list queries from a
    /// fixed record set, decoding the request exactly as the real service.
    struct Fixture {
        records: Vec<MountRecord>,
        malformed: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<MountRecord>) -> Self {
            Self {
                records,
                malformed: false,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.malformed {
                return Ok(alloc::vec![0u8; MountRecord::WIRE_LEN + 1]);
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

    fn record(source: &[u8], target: &[u8], fstype: &[u8], flags: MountFlags) -> MountRecord {
        MountRecord::new(
            source,
            target,
            fstype,
            flags,
            MountVolumeState {
                usage: VolumeStats::default(),
                availability: MountAvailability::Available,
                medium: None,
            },
            [0u8; 16],
        )
        .expect("record")
    }

    #[test]
    fn the_storage_medium_survives_the_client_decode() {
        // The medium the service reported reaches the consumer through the
        // paging walk, so a caller renders the drive it is really on.
        let record = MountRecord::new(
            b"usb1",
            b"/Storage/usb1",
            b"fat32",
            MountFlags::NOSUID,
            MountVolumeState {
                usage: VolumeStats::default(),
                availability: MountAvailability::Available,
                medium: Some(BlkDeviceClass::Removable),
            },
            [0u8; 16],
        )
        .expect("record");
        let fixture = Fixture::new(alloc::vec![record]);
        let seen = collect(&fixture).expect("walk");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].medium(), Some(BlkDeviceClass::Removable));
    }

    fn collect(fixture: &Fixture) -> Result<Vec<MountRecord>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_mount(fixture, |r| {
            seen.borrow_mut().push(*r);
            Ok(WalkStep::Continue)
        })?;
        Ok(seen.into_inner())
    }

    #[test]
    fn walk_routes_the_mount_query_and_yields_records() {
        let fixture = Fixture::new(alloc::vec![
            record(b"rootfs", b"/", b"arxfs", MountFlags::READ_ONLY),
            record(b"data", b"/Storage/data", b"arxfs", MountFlags::default()),
        ]);
        let got = collect(&fixture).expect("ok");
        assert_eq!(got.len(), 2);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::MOUNT_LIST]
        );
    }

    #[test]
    fn walk_pages_until_a_short_page() {
        let mut records = Vec::new();
        for _ in 0..=MOUNT_PAGE {
            records.push(record(b"v", b"/Storage/v", b"arxfs", MountFlags::default()));
        }
        let fixture = Fixture::new(records);
        let got = collect(&fixture).expect("ok");
        assert_eq!(got.len(), usize::from(MOUNT_PAGE) + 1);
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn empty_table_yields_nothing() {
        let fixture = Fixture::new(Vec::new());
        assert!(collect(&fixture).expect("ok").is_empty());
        assert_eq!(fixture.seen.borrow().len(), 1);
    }

    #[test]
    fn malformed_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(
            b"rootfs",
            b"/",
            b"arxfs",
            MountFlags::default()
        )]);
        fixture.malformed = true;
        assert_eq!(
            collect(&fixture),
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
    }

    #[test]
    fn sink_error_stops_the_walk() {
        let fixture = Fixture::new(alloc::vec![
            record(b"a", b"/a", b"arxfs", MountFlags::default()),
            record(b"b", b"/b", b"arxfs", MountFlags::default()),
        ]);
        let count = RefCell::new(0usize);
        let result = for_each_mount(&fixture, |_| {
            *count.borrow_mut() += 1;
            Err(Errno::NotFound)
        });
        assert_eq!(result, Err(ListError::Sink(Errno::NotFound)));
        assert_eq!(*count.borrow(), 1);
    }

    #[test]
    fn render_writes_the_classic_mount_line() {
        let flags = MountFlags::NOSUID
            .union(MountFlags::NODEV)
            .union(MountFlags::NOEXEC);
        let line = render_mount(&record(b"data", b"/Storage/data", b"arxfs", flags));
        assert_eq!(
            line,
            "data on /Storage/data type arxfs (rw,nosuid,nodev,noexec)"
        );
    }

    #[test]
    fn an_unavailable_volume_is_marked_in_the_render() {
        for (availability, marker) in [
            (MountAvailability::UnavailableDirty, "[unavailable-dirty]"),
            (MountAvailability::RecoveryConflict, "[recovery-conflict]"),
            (MountAvailability::UnavailableLost, "[unavailable-lost]"),
        ] {
            let record = MountRecord::new(
                b"usb1",
                b"/Storage/usb1",
                b"fat32",
                MountFlags::NOSUID,
                MountVolumeState {
                    usage: VolumeStats::default(),
                    availability,
                    medium: None,
                },
                [7u8; 16],
            )
            .expect("record");
            let line = render_mount(&record);
            assert_eq!(
                line,
                alloc::format!("usb1 on /Storage/usb1 type fat32 (rw,nosuid) {marker}")
            );
        }
    }

    #[test]
    fn read_only_renders_ro() {
        let line = render_mount(&record(b"rootfs", b"/", b"arxfs", MountFlags::READ_ONLY));
        assert_eq!(line, "rootfs on / type arxfs (ro)");
    }

    #[test]
    fn render_options_orders_ro_then_restrictions() {
        assert_eq!(render_options(MountFlags::default()), "rw");
        assert_eq!(render_options(MountFlags::READ_ONLY), "ro");
        assert_eq!(
            render_options(MountFlags::NODEV.union(MountFlags::READ_ONLY)),
            "ro,nodev"
        );
    }

    #[test]
    fn render_is_lossy_on_invalid_bytes() {
        let line = render_mount(&record(
            &[0xFF, 0xFE],
            b"/",
            b"arxfs",
            MountFlags::default(),
        ));
        assert!(line.contains('\u{FFFD}'));
    }
}
