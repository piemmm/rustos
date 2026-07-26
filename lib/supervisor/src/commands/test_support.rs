//! In-memory test doubles for the Supervisor seams.
//!
//! These let every command and the REPL be driven entirely on the host: a
//! [`VecReport`] captures rendered output, a [`MockInput`] replays scripted
//! keyboard bytes, and a [`MockHost`] returns canned data and records the
//! control actions and audit events it was asked to perform. They exist only
//! under `cfg(test)` and never reach a TAIRiX build.

use alloc::string::String;
use alloc::vec::Vec;

use crate::dispatch::Session;
use crate::{MountOutcome, Report, SupInput, SupervisorEvent, SupervisorHost, TestOutcome};

/// A [`Report`] that accumulates everything written to it.
#[derive(Default)]
pub struct VecReport {
    bytes: Vec<u8>,
}

impl VecReport {
    /// The captured output as a byte slice.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether the captured output contains `needle` (as a UTF-8 substring).
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.bytes
            .windows(needle.len().max(1))
            .any(|window| window == needle.as_bytes())
            || (needle.is_empty())
    }
}

impl Report for VecReport {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
}

/// A [`SupInput`] that replays a fixed script of bytes then reports EOF.
pub struct MockInput {
    script: Vec<u8>,
    pos: usize,
}

impl MockInput {
    /// A reader that will hand out `script` byte by byte.
    #[must_use]
    pub fn new(script: &[u8]) -> Self {
        Self {
            script: script.to_vec(),
            pos: 0,
        }
    }
}

impl SupInput for MockInput {
    fn read_byte(&mut self) -> Option<u8> {
        let byte = self.script.get(self.pos).copied();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }
}

/// A [`SupervisorHost`] returning canned data and recording actions.
#[derive(Default)]
pub struct MockHost {
    /// Every audit event the engine emitted, in order.
    pub audits: Vec<SupervisorEvent>,
    /// Whether `reboot` was called.
    pub rebooted: bool,
    /// Whether `poweroff` was called.
    pub powered_off: bool,
    mount_result: Option<MountOutcome>,
    memtest_result: Option<TestOutcome>,
    last_memtest_passes: Option<u32>,
    scan_result: Option<TestOutcome>,
    last_scan_device: Option<String>,
}

impl MockHost {
    /// Set the result the next `mount` returns.
    pub fn set_mount_result(&mut self, result: MountOutcome) {
        self.mount_result = Some(result);
    }

    /// Set the result the next `memtest` returns.
    pub fn set_memtest_result(&mut self, result: TestOutcome) {
        self.memtest_result = Some(result);
    }

    /// Set the result the next `test disk` returns.
    pub fn set_scan_result(&mut self, result: TestOutcome) {
        self.scan_result = Some(result);
    }

    /// The pass count the most recent `memtest` was asked for.
    #[must_use]
    pub fn last_memtest_passes(&self) -> Option<u32> {
        self.last_memtest_passes
    }

    /// The device the most recent `test disk` scanned.
    #[must_use]
    pub fn last_scan_device(&self) -> Option<String> {
        self.last_scan_device.clone()
    }
}

impl SupervisorHost for MockHost {
    fn version(&mut self, out: &mut dyn Report) {
        out.line("TAIRiX kernel 0.0.0 (test) target=host abi=v1");
    }

    fn memory(&mut self, out: &mut dyn Report) {
        out.write_str("usable RAM: ");
        out.write_u64(1024);
        out.line(" MiB; heap 8 MiB; pressure: low");
    }

    fn memory_map(&mut self, out: &mut dyn Report) {
        out.line("boot memory map:");
        out.line("  0x0 - 0x100000 reserved");
    }

    fn cpu(&mut self, out: &mut dyn Report) {
        out.line("CPU: 4 cores; features: (test)");
    }

    fn hardware(&mut self, out: &mut dyn Report) {
        out.line("hardware tree:");
        out.line("  node 0 class=bus");
    }

    fn disks(&mut self, out: &mut dyn Report) {
        out.line("disk0: 64 GiB (test)");
    }

    fn partitions(&mut self, device: &str, out: &mut dyn Report) {
        out.write_str("partitions of ");
        out.line(device);
        out.line("  1: ARXFSRoot");
    }

    fn arxfs_status(&mut self, out: &mut dyn Report) {
        out.line("ARXFS root: present, locked");
    }

    fn list(&mut self, path: Option<&str>, out: &mut dyn Report) {
        out.write_str("listing ");
        out.line(path.unwrap_or("/System"));
        out.line("  Apps  Kernel  Security");
    }

    fn log_tail(&mut self, _count: Option<usize>, out: &mut dyn Report) {
        out.line("boot-log:");
        out.line("  4136 root-unlock installed");
    }

    fn panic_log(&mut self, out: &mut dyn Report) {
        out.line("panic-log: no previous crash recorded");
    }

    fn uptime(&mut self, out: &mut dyn Report) {
        out.line("uptime: 0d 00:00:12");
    }

    fn date(&mut self, out: &mut dyn Report) {
        out.line("date: 1970-01-01 00:00:12 UTC");
    }

    fn memtest(
        &mut self,
        passes: u32,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        self.last_memtest_passes = Some(passes);
        // Exercise the abort predicate exactly as the real host would.
        let _ = abort();
        out.line("memtest: running...");
        self.memtest_result.unwrap_or(TestOutcome::Passed)
    }

    fn scan_disk(
        &mut self,
        device: &str,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        self.last_scan_device = Some(device.into());
        let _ = abort();
        out.line("scanning...");
        self.scan_result.unwrap_or(TestOutcome::Passed)
    }

    fn mount(&mut self, _passphrase: &[u8], _out: &mut dyn Report) -> MountOutcome {
        self.mount_result.unwrap_or(MountOutcome::Failed)
    }

    fn reboot(&mut self) {
        self.rebooted = true;
    }

    fn poweroff(&mut self) {
        self.powered_off = true;
    }

    fn audit(&mut self, event: SupervisorEvent) {
        self.audits.push(event);
    }
}

/// A bundle of the three mocks that hands out a [`Session`] and answers the
/// assertions the command tests make.
pub struct MockSession {
    out: VecReport,
    input: MockInput,
    host: MockHost,
}

impl MockSession {
    /// A session whose keyboard replays `script`.
    #[must_use]
    pub fn new(script: &[u8]) -> Self {
        Self {
            out: VecReport::default(),
            input: MockInput::new(script),
            host: MockHost::default(),
        }
    }

    /// Borrow the three mocks as a [`Session`] for one dispatch.
    pub fn session(&mut self) -> Session<'_> {
        Session {
            out: &mut self.out,
            input: &mut self.input,
            host: &mut self.host,
        }
    }

    /// The rendered output as bytes.
    #[must_use]
    pub fn output_bytes(&self) -> &[u8] {
        self.out.bytes()
    }

    /// The rendered output (empty check helper).
    #[must_use]
    pub fn output(&self) -> &[u8] {
        self.out.bytes()
    }

    /// Whether the rendered output contains `needle`.
    #[must_use]
    pub fn output_contains(&self, needle: &str) -> bool {
        self.out.contains(needle)
    }

    /// Whether `event` was audited.
    #[must_use]
    pub fn audited(&self, event: SupervisorEvent) -> bool {
        self.host.audits.contains(&event)
    }

    /// Whether a reboot was requested.
    #[must_use]
    pub fn rebooted(&self) -> bool {
        self.host.rebooted
    }

    /// Whether a power-off was requested.
    #[must_use]
    pub fn powered_off(&self) -> bool {
        self.host.powered_off
    }

    /// Set the next `mount` result.
    pub fn set_mount_result(&mut self, result: MountOutcome) {
        self.host.set_mount_result(result);
    }

    /// Set the next `memtest` result.
    pub fn set_memtest_result(&mut self, result: TestOutcome) {
        self.host.set_memtest_result(result);
    }

    /// Set the next `test disk` result.
    pub fn set_scan_result(&mut self, result: TestOutcome) {
        self.host.set_scan_result(result);
    }

    /// The pass count the last `memtest` ran.
    #[must_use]
    pub fn last_memtest_passes(&self) -> Option<u32> {
        self.host.last_memtest_passes()
    }

    /// The device the last `test disk` scanned.
    #[must_use]
    pub fn last_scan_device(&self) -> Option<String> {
        self.host.last_scan_device()
    }
}
