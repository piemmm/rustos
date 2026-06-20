//! The `Run` entry-point binary of the device-manager service, installed
//! at `/System/Services/devmgr` (`AGENTS.md` §16.2, §18.3) — the
//! long-running user-space service PID 1 `init` launches to observe the
//! discovered hardware tree and react to it.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only (`AGENTS.md` §1),
//! so it links the Rust userland runtime `rustos-rt` — never the C ABI,
//! which exists solely for programs **not** written in Rust (`AGENTS.md`
//! §16.4). `rustos-rt` provides `_start`, the per-process stack canary
//! (`AGENTS.md` §19.2), the panic handler, and the syscall wrappers
//! (`hw_tree_read` / `hw_tree_wait`); `rustos_rt::entry!` names this
//! program's `main`.
//!
//! # What this service does (Design D D2b-2c)
//!
//! At startup it fetches the kernel-decoded driver **catalogue** over the
//! capability-gated `ipc_call` endpoint the kernel store service serves
//! (`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`, `AGENTS.md` §18.3 /
//! §5.2): one entry per installed bundle, an opaque `bundle_id` plus the
//! bind table the kernel decoded from its signed manifest. The store is
//! read-only and static for the life of the system, so the catalogue is
//! fetched **once** (`AGENTS.md` §2.16); a fetch failure is fail-soft
//! (`AGENTS.md` §18.4 / §2.9).
//!
//! It then reads the architecture-neutral hardware tree the kernel
//! discovered at boot through the capability-gated `hw_tree_read` syscall
//! (`CAP_SYSINFO_HW`, `AGENTS.md` §16.6 / §18.4), matches each node against
//! the catalogue with the shared [`rustos_devmatch`] policy (`AGENTS.md`
//! §18.3), and asks the kernel to load the matched bundle for each winning
//! node (`StoreRequest::Load`) — the kernel re-runs the signed §8 gate and
//! spawns the driver with only that node's grants (`AGENTS.md` §4). It then
//! **blocks** in `hw_tree_wait` until the tree changes (a node seeded,
//! appended, or removed — `AGENTS.md` §18.4) and re-matches, loading each
//! newly-appeared node's driver once. It never busy-spins (`AGENTS.md`
//! §2.1): the wait parks the task in the kernel until the store's generation
//! advances. The device manager owns matching *policy* only; the kernel
//! keeps the load *mechanism* (bytes, signature, spawn) in its TCB
//! (`AGENTS.md` §4).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the
// optional `rustos-rt` runtime through the default `program` feature. The
// kernel links this crate's *library* with `default-features = false`, so
// it never builds this module (nor pulls in `rustos-rt`, `AGENTS.md`
// §17.4).
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use rustos_abi::driver_store::DRIVER_STORE_ENDPOINT;
    use rustos_abi::hwtree::HwDeviceClass;
    use rustos_abi::{Errno, HwNode, HwTreeHeader};
    use rustos_devmgr::{DriverStoreCall, HwTreeService};
    use rustos_log::{Event, Sink};
    use rustos_util::fmt::{format_hex_u64, format_usize};

    /// Buffer the discovered tree is read into. Sized as a generous §24.2
    /// headroom default — a `HwNode` is `HwNode::WIRE_LEN` (572) bytes, so
    /// 64 KiB holds ~114 nodes, far more than any discovered floor tree —
    /// not a hard ceiling on the inventory. An over-large tree fails closed
    /// (`Errno::BufferTooSmall`, `AGENTS.md` §2.9 / §24.1) rather than
    /// truncating the inventory.
    const READ_BUF_LEN: usize = 64 * 1024;

    /// Buffer the catalogue and each `Load` reply are received into, sized to
    /// the endpoint's `DRIVER_STORE_MAX_REPLY` so a full catalogue is never
    /// truncated (`AGENTS.md` §2.9 / §24.1). Disjoint from the tree buffer so
    /// a load can run while a tree snapshot is being decoded.
    const REPLY_BUF_LEN: usize = 64 * 1024;

    /// Write all of `bytes` to standard error (fd 2 — diagnostics,
    /// `AGENTS.md` §20), looping over short writes. A write that accepts
    /// zero bytes means the stream will accept no more; the loop stops
    /// rather than spinning (`AGENTS.md` §2.1).
    fn write_all_stderr(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = rustos_rt::stderr(bytes);
            if written == 0 {
                break;
            }
            bytes = &bytes[written.min(bytes.len())..];
        }
    }

    /// The stable name of a node's device class for the log line. An
    /// unknown (un-modelled) discriminant is reported as `?`, never
    /// guessed (`AGENTS.md` §2.9).
    fn class_name(class: Option<HwDeviceClass>) -> &'static str {
        match class {
            Some(HwDeviceClass::Root) => "root",
            Some(HwDeviceClass::Bus) => "bus",
            Some(HwDeviceClass::Cpu) => "cpu",
            Some(HwDeviceClass::Memory) => "memory",
            Some(HwDeviceClass::Timer) => "timer",
            Some(HwDeviceClass::InterruptController) => "intc",
            Some(HwDeviceClass::Display) => "display",
            Some(HwDeviceClass::Input) => "input",
            Some(HwDeviceClass::Network) => "network",
            Some(HwDeviceClass::Storage) => "storage",
            Some(HwDeviceClass::Serial) => "serial",
            Some(HwDeviceClass::Other) => "other",
            None => "?",
        }
    }

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`). An unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// The production [`HwTreeService`] backing: the reactive observe loop
    /// ([`rustos_devmgr::run`]) reads, waits, and reports through this seam,
    /// which binds the `hw_tree_read` / `hw_tree_wait` `abi-v1` syscalls and
    /// writes each node report to the inherited diagnostic stream (fd 2,
    /// `AGENTS.md` §20). The loop's control flow is host-tested in
    /// `rustos_devmgr::service`; this is the freestanding I/O it binds
    /// (`AGENTS.md` §2.2).
    struct RtTreeService;

    impl HwTreeService for RtTreeService {
        fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            rustos_rt::hw_tree_read(buf).map_err(errno_from)
        }

        fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno> {
            // Block until the tree's generation advances past the one last
            // observed; `u64::MAX` is the effectively unbounded wait a
            // device manager holds for the life of the system. A negative
            // return is a `-errno` the loop fails closed on (`AGENTS.md`
            // §2.9).
            let waited = rustos_rt::hw_tree_wait(last_generation, u64::MAX);
            if waited < 0 {
                return Err(errno_from(waited));
            }
            Ok(())
        }

        fn on_header(&mut self, header: &HwTreeHeader) {
            let mut count = [0u8; 12];
            let mut gen = [0u8; 16];
            write_all_stderr(b"devmgr: hardware tree generation ");
            write_all_stderr(format_hex_u64(header.generation(), &mut gen).as_bytes());
            write_all_stderr(b" nodes ");
            write_all_stderr(format_usize(header.node_count() as usize, &mut count).as_bytes());
            write_all_stderr(b"\n");
        }

        fn on_node(&mut self, node: &HwNode) {
            let mut id = [0u8; 12];
            let mut parent = [0u8; 12];
            let mut keys = [0u8; 12];
            write_all_stderr(b"devmgr: node ");
            write_all_stderr(format_usize(node.id() as usize, &mut id).as_bytes());
            write_all_stderr(b" parent ");
            // The root has no parent (the all-ones sentinel); name it `root`
            // rather than render a meaningless huge number.
            if node.is_root() {
                write_all_stderr(b"root");
            } else {
                write_all_stderr(format_usize(node.parent() as usize, &mut parent).as_bytes());
            }
            write_all_stderr(b" class ");
            write_all_stderr(class_name(node.class()).as_bytes());
            write_all_stderr(b" keys ");
            write_all_stderr(format_usize(node.match_keys().len(), &mut keys).as_bytes());
            write_all_stderr(b"\n");
        }
    }

    /// The production [`DriverStoreCall`] backing: it binds the `ipc_call`
    /// `abi-v1` syscall to the read-only `/System` driver-store endpoint
    /// ([`DRIVER_STORE_ENDPOINT`]) the kernel store service serves
    /// (`AGENTS.md` §18.3 / §5.2). The protocol logic (request framing, reply
    /// decoding) is host-tested in `rustos_devmgr::store`; this is the
    /// freestanding I/O it binds (`AGENTS.md` §2.2). The kernel re-checks the
    /// caller's `CAP_DRV_LOAD` on every call; this client adds no authority.
    struct RtStoreCall;

    impl DriverStoreCall for RtStoreCall {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // `ipc_call` returns the raw `-errno` on failure; recover the
            // typed `Errno` and surface it fail-closed (`AGENTS.md` §2.9).
            rustos_rt::ipc_call(DRIVER_STORE_ENDPOINT, request, reply).map_err(errno_from)
        }
    }

    /// The production audit [`Sink`]: renders each match/load decision to the
    /// inherited diagnostic stream (fd 2, `AGENTS.md` §20). The reactive
    /// loop's audit records are host-tested in `rustos_devmgr::autoload`;
    /// this is the freestanding rendering it binds (`AGENTS.md` §2.2).
    struct StderrSink;

    impl Sink for StderrSink {
        fn write_event(&self, event: &Event<'_>) {
            let mut id = [0u8; 12];
            write_all_stderr(b"devmgr: [");
            write_all_stderr(format_usize(event.id.0 as usize, &mut id).as_bytes());
            write_all_stderr(b"] ");
            write_all_stderr(event.message.as_bytes());
            for field in event.fields {
                write_all_stderr(b" ");
                write_all_stderr(field.key.as_bytes());
                write_all_stderr(b"=");
                write_all_stderr(field.value.as_bytes());
            }
            write_all_stderr(b"\n");
        }
    }

    /// Program entry point. Runs the reactive match-and-load loop for the
    /// life of the service (`budget = None`): fetch the catalogue once, then
    /// read the discovered tree, load a driver for every matched node, and
    /// block on every generation advance to re-match (`AGENTS.md` §18.4). The
    /// loop returns only on a fail-closed tree-seam error; PID 1 `init`
    /// supervises and relaunches the service.
    fn main() -> i32 {
        // Two persistent stack buffers: the tree snapshot and the
        // catalogue/load reply. The §16.5 stack sizing
        // (`spawn_layout::USER_STACK_PAGES`, ~1.1 MiB) covers both 64 KiB
        // buffers comfortably; they are disjoint so a load reply never
        // clobbers the tree snapshot mid-decode (`AGENTS.md` §2.16).
        let mut tree_buf = [0u8; READ_BUF_LEN];
        let mut reply_buf = [0u8; REPLY_BUF_LEN];
        match rustos_devmgr::run(
            &mut RtTreeService,
            &mut RtStoreCall,
            &StderrSink,
            &mut tree_buf,
            &mut reply_buf,
            None,
        ) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `rustos-rt` `_start` path is not compiled
// — on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
