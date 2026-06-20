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
//! # What this service does (Design D foundation)
//!
//! At startup it lists the read-only `/System/Drivers/` driver store over
//! the capability-gated `ipc_call` endpoint the kernel store service serves
//! (`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`, `AGENTS.md` §18.3 /
//! §5.2) and logs every installed bundle path — fail-soft if no store is
//! served (`AGENTS.md` §18.4 / §2.9). The store is read-only and static for
//! the life of the system, so it is listed **once**, not on every hotplug
//! generation (`AGENTS.md` §2.16).
//!
//! It then reads the architecture-neutral hardware tree the kernel
//! discovered at boot through the capability-gated `hw_tree_read` syscall
//! (`CAP_SYSINFO_HW`, `AGENTS.md` §16.6 / §18.4), logs every node, then
//! **blocks** in `hw_tree_wait` until the tree changes (a node seeded,
//! appended, or removed — `AGENTS.md` §18.4) and re-reads it. It never
//! busy-spins (`AGENTS.md` §2.1): the wait parks the task in the kernel
//! until the store's generation advances.
//!
//! **Matching is not done here yet.** Resolving each discovered node against
//! the store bundles' bind tables and loading the winners
//! (`driver_store_load`) is the **next** tranche; the in-kernel single-pass
//! autoload still performs the loads, so this service only observes (the hw
//! tree and the store listing) and reacts. The match policy already lives in
//! [`rustos_devmgr::resolve`] (`AGENTS.md` §18.3), but it consumes
//! *decoded* bind tables — decoding a bundle's signed manifest is the
//! driver-host load gate's job (`ParsedImage::decode_bind_table`), which
//! sits outside this `lib/*`-only crate's dependencies (§17.4), so the
//! match step lands with that wiring in the next tranche.
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
    use rustos_devmgr::{list_store, DriverStoreCall, HwTreeService};
    use rustos_util::fmt::{format_hex_u64, format_usize};

    /// Buffer the discovered tree is read into. Sized as a generous §24.2
    /// headroom default — a `HwNode` is `HwNode::WIRE_LEN` (572) bytes, so
    /// 64 KiB holds ~114 nodes, far more than any discovered floor tree —
    /// not a hard ceiling on the inventory. Growing it on `BufferTooSmall`
    /// needs the `mem_map`-backed userland heap, whose production producer
    /// is still staged (`plans/SPAWN.md` SP5b); until then an
    /// over-large tree is reported and the service fails closed
    /// (`AGENTS.md` §2.9 / §24.1) rather than truncating the inventory.
    const READ_BUF_LEN: usize = 64 * 1024;

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

    /// List the read-only `/System/Drivers/` store over the `ipc_call`
    /// endpoint and report every installed bundle path to the diagnostic
    /// stream (fd 2, `AGENTS.md` §20), reusing `buf` for the reply.
    ///
    /// Fail-soft (`AGENTS.md` §18.4 / §2.9): if the store endpoint is not
    /// bound (no `/System` volume served) or the listing fails, it reports
    /// the condition and returns — it never aborts the service, mirroring the
    /// kernel server's own fail-closed-but-non-fatal store handling. Design D
    /// (a) is observe-only: the in-kernel autoload still performs the loads,
    /// so this logs the discovered store without matching or loading.
    fn report_store(buf: &mut [u8]) {
        match list_store(&mut RtStoreCall, buf) {
            Ok(paths) => {
                let mut count = [0u8; 12];
                write_all_stderr(b"devmgr: driver store bundles ");
                write_all_stderr(format_usize(paths.len(), &mut count).as_bytes());
                write_all_stderr(b"\n");
                for path in &paths {
                    write_all_stderr(b"devmgr: store bundle ");
                    write_all_stderr(path.as_bytes());
                    write_all_stderr(b"\n");
                }
            }
            Err(err) => {
                let mut code = [0u8; 12];
                write_all_stderr(b"devmgr: driver store unavailable errno ");
                write_all_stderr(format_usize(err as usize, &mut code).as_bytes());
                write_all_stderr(b"\n");
            }
        }
    }

    /// Program entry point. Lists and logs the read-only `/System` driver
    /// store once (`AGENTS.md` §18.3), then runs the reactive observe loop
    /// for the life of the service (`budget = None`): read and log the
    /// discovered tree, then block on every generation advance and re-read it
    /// (`AGENTS.md` §18.4). The loop returns only on a fail-closed error;
    /// PID 1 `init` supervises and relaunches the service.
    fn main() -> i32 {
        // The read buffer is a single persistent allocation on the stack;
        // the §16.5 stack sizing (`spawn_layout::USER_STACK_PAGES`) covers
        // it comfortably. It is reused for the one-shot store listing (which
        // completes before the loop) and then for every tree read — the
        // read-only `/System` store is static for the life of the system, so
        // it is listed once, not on every hotplug generation (`AGENTS.md`
        // §2.16).
        let mut buf = [0u8; READ_BUF_LEN];
        report_store(&mut buf);
        match rustos_devmgr::run(&mut RtTreeService, &mut buf, None) {
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
