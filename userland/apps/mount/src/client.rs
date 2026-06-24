//! The mount engine: list the mount table through the System Information
//! API, or hand a parsed attach request to the kernel.

use rustos_procinfo::{for_each_mount, render_mount, Output, Transport};

use crate::command::Command;
use crate::error::MountError;
use crate::io::{MountSpec, Mounter};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: mount [-r] [-t TYPE] [-o OPTIONS] [--] [SOURCE TARGET]

  (no operands)        list the mounted filesystems
  SOURCE TARGET        mount SOURCE at TARGET (needs CAP_FS_MOUNT)
  -r, --read-only      mount read-only (same as -o ro)
  -t, --types TYPE     filesystem type (probed when omitted)
  -o, --options LIST   comma-separated: ro,rw,nosuid,nodev,noexec
  -h, --help           show this message

`--` ends option parsing: every later argument is an operand.";

/// Run one [`Command`].
///
/// A [`Command::List`] pages the mount table through `transport` (the
/// ungated `MOUNT_LIST` query) and writes one line per
/// mount to `out`. A [`Command::Mount`] hands the parsed request to
/// `mounter`; the capability gate lives in the kernel, not here, so a denied mount comes back as
/// [`MountError::Mount`]. `mount` writes nothing on a successful attach.
///
/// # Errors
///
/// * [`MountError::Mount`] — the kernel refused or failed the attach (e.g. a
///   missing `CAP_FS_MOUNT`, an unknown source, or a bad superblock).
/// * [`MountError::Service`] — the transport failed or the reply did not
///   decode against `sysinfo-v1` while listing.
/// * [`MountError::Output`] — writing the terminal failed.
pub fn run(
    command: Command,
    mounter: &dyn Mounter,
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), MountError> {
    match command {
        Command::Help => out.write_line(USAGE).map_err(MountError::Output),
        Command::List => run_list(transport, out),
        Command::Mount(request) => {
            let spec = MountSpec {
                source: &request.source,
                target: &request.target,
                fstype: request.fstype.as_deref(),
                flags: request.flags,
            };
            mounter.mount(&spec).map_err(MountError::Mount)
        }
    }
}

/// Page through the mount table and render one line per mounted filesystem.
fn run_list(transport: &dyn Transport, out: &dyn Output) -> Result<(), MountError> {
    for_each_mount(transport, |record| out.write_line(&render_mount(record)))
        .map_err(MountError::from)
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::{parse, Command};
    use crate::error::MountError;
    use crate::io::{MountSpec, Mounter};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::driver::filesystem::MountFlags;
    use rustos_abi::sysinfo::{
        MountListRequest, MountRecord, SysinfoQueryId, SysinfoRequestHeader,
    };
    use rustos_abi::Errno;
    use rustos_procinfo::{Output, Transport};

    /// An in-memory `sysinfod` stand-in answering mount-list queries from a
    /// fixture, decoding the request the same way the real service does.
    struct Fixture {
        records: Vec<MountRecord>,
        fail: Option<Errno>,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<MountRecord>) -> Self {
            Self {
                records,
                fail: None,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
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

    /// Captures rendered lines.
    struct Recorder {
        lines: RefCell<Vec<String>>,
    }
    impl Recorder {
        fn new() -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
            }
        }
        fn lines(&self) -> Vec<String> {
            self.lines.borrow().clone()
        }
    }
    impl Output for Recorder {
        fn write_line(&self, line: &str) -> Result<(), Errno> {
            self.lines.borrow_mut().push(line.to_string());
            Ok(())
        }
    }

    /// An output seam that always fails.
    struct FailingOutput;
    impl Output for FailingOutput {
        fn write_line(&self, _line: &str) -> Result<(), Errno> {
            Err(Errno::PermissionDenied)
        }
    }

    /// One recorded attach request.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Attached {
        source: String,
        target: String,
        fstype: Option<String>,
        flags: MountFlags,
    }

    /// Records every attach request, optionally failing it.
    struct MemMounter {
        fail: Option<Errno>,
        attached: RefCell<Vec<Attached>>,
    }
    impl MemMounter {
        fn new() -> Self {
            Self {
                fail: None,
                attached: RefCell::new(Vec::new()),
            }
        }
        fn failing(errno: Errno) -> Self {
            Self {
                fail: Some(errno),
                attached: RefCell::new(Vec::new()),
            }
        }
    }
    impl Mounter for MemMounter {
        fn mount(&self, spec: &MountSpec<'_>) -> Result<(), Errno> {
            if let Some(errno) = self.fail {
                return Err(errno);
            }
            self.attached.borrow_mut().push(Attached {
                source: spec.source.to_string(),
                target: spec.target.to_string(),
                fstype: spec.fstype.map(String::from),
                flags: spec.flags,
            });
            Ok(())
        }
    }

    fn record(source: &[u8], target: &[u8], fstype: &[u8], flags: MountFlags) -> MountRecord {
        MountRecord::new(source, target, fstype, flags).expect("record")
    }

    #[test]
    fn help_prints_usage_and_touches_no_query() {
        let mounter = MemMounter::new();
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Help, &mounter, &fixture, &out), Ok(()));
        assert_eq!(out.lines(), alloc::vec![USAGE.to_string()]);
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn list_renders_one_line_per_mount_and_routes_the_query() {
        let mounter = MemMounter::new();
        let fixture = Fixture::new(alloc::vec![
            record(b"rootfs", b"/", b"rustfs", MountFlags::READ_ONLY),
            record(b"data", b"/Storage/data", b"rustfs", MountFlags::NOSUID),
        ]);
        let out = Recorder::new();
        assert_eq!(run(Command::List, &mounter, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "rootfs on / type rustfs (ro)");
        assert_eq!(lines[1], "data on /Storage/data type rustfs (rw,nosuid)");
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::MOUNT_LIST]
        );
        // Listing attaches nothing.
        assert!(mounter.attached.borrow().is_empty());
    }

    #[test]
    fn empty_table_lists_nothing() {
        let mounter = MemMounter::new();
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::List, &mounter, &fixture, &out), Ok(()));
        assert!(out.lines().is_empty());
    }

    #[test]
    fn a_list_transport_error_surfaces_as_service() {
        let mounter = MemMounter::new();
        let mut fixture = Fixture::new(Vec::new());
        fixture.fail = Some(Errno::NotFound);
        let out = Recorder::new();
        assert_eq!(
            run(Command::List, &mounter, &fixture, &out),
            Err(MountError::Service(Errno::NotFound))
        );
    }

    #[test]
    fn a_list_output_failure_surfaces() {
        let mounter = MemMounter::new();
        let fixture = Fixture::new(alloc::vec![record(
            b"rootfs",
            b"/",
            b"rustfs",
            MountFlags::default()
        )]);
        assert_eq!(
            run(Command::List, &mounter, &fixture, &FailingOutput),
            Err(MountError::Output(Errno::PermissionDenied))
        );
    }

    fn run_mount(args: &[&str], mounter: &MemMounter) -> Result<(), MountError> {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(parse(args).expect("valid command"), mounter, &fixture, &out)
    }

    #[test]
    fn a_mount_request_reaches_the_kernel() {
        let mounter = MemMounter::new();
        assert_eq!(
            run_mount(&["-r", "-t", "rustfs", "vol", "/Storage/vol"], &mounter),
            Ok(())
        );
        let attached = mounter.attached.borrow();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].source, "vol");
        assert_eq!(attached[0].target, "/Storage/vol");
        assert_eq!(attached[0].fstype.as_deref(), Some("rustfs"));
        assert!(attached[0].flags.contains(MountFlags::READ_ONLY));
    }

    #[test]
    fn a_denied_mount_surfaces_as_mount_permission_denied() {
        // The kernel is the policy point: a missing CAP_FS_MOUNT is its call
        // to make, surfaced here as Mount(PermissionDenied).
        let mounter = MemMounter::failing(Errno::PermissionDenied);
        assert_eq!(
            run_mount(&["vol", "/Storage/vol"], &mounter),
            Err(MountError::Mount(Errno::PermissionDenied))
        );
        assert!(mounter.attached.borrow().is_empty());
    }

    #[test]
    fn a_mount_with_no_fstype_passes_none() {
        let mounter = MemMounter::new();
        assert_eq!(run_mount(&["vol", "/Storage/vol"], &mounter), Ok(()));
        assert_eq!(mounter.attached.borrow()[0].fstype, None);
    }

    #[test]
    fn a_help_write_failure_surfaces() {
        let mounter = MemMounter::new();
        let fixture = Fixture::new(Vec::new());
        assert_eq!(
            run(Command::Help, &mounter, &fixture, &FailingOutput),
            Err(MountError::Output(Errno::PermissionDenied))
        );
    }
}
