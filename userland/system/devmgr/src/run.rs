//! The `Run` entry-point binary of the device-manager service, installed
//! at `/System/Services/devmgr` — the
//! long-running user-space service PID 1 `init` launches to observe the
//! discovered hardware tree and react to it.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only,
//! so it links the Rust userland runtime `rustos-rt` — never the C ABI,
//! which exists solely for programs **not** written in Rust. `rustos-rt` provides `_start`, the per-process stack canary, the panic handler, and the syscall wrappers
//! (`hw_tree_read` / `hw_tree_wait`); `rustos_rt::entry!` names this
//! program's `main`.
//!
//! # What this service does (Design D D2b-2c)
//!
//! At startup it fetches the kernel-decoded driver **catalogue** over the
//! capability-gated `ipc_call` endpoint the kernel store service serves
//! (`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`): one entry per installed bundle, an opaque `bundle_id` plus the
//! bind table the kernel decoded from its signed manifest. The store is
//! read-only and static for the life of the system, so the catalogue is
//! fetched **once**; a fetch failure is fail-soft.
//!
//! It then reads the architecture-neutral hardware tree the kernel
//! discovered at boot through the capability-gated `hw_tree_read` syscall
//! (`CAP_SYSINFO_HW`), matches each node against
//! the catalogue with the shared [`rustos_devmatch`] policy, and asks the kernel to load the matched bundle for each winning
//! node (`StoreRequest::Load`) — the kernel re-runs the signed gate and
//! spawns the driver with only that node's grants. It then
//! **blocks** in `hw_tree_wait` until the tree changes (a node seeded,
//! appended, or removed) and re-matches, loading each
//! newly-appeared node's driver once. It never busy-spins: the wait parks the task in the kernel until the store's generation
//! advances. The device manager owns matching *policy* only; the kernel
//! keeps the load *mechanism* (bytes, signature, spawn) in its TCB.
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
// it never builds this module (nor pulls in `rustos-rt`).
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use rustos_abi::driver_store::DRIVER_STORE_ENDPOINT;
    use rustos_abi::hwtree::HwDeviceClass;
    use rustos_abi::{Errno, HwNode, HwTreeHeader};
    use rustos_devmgr::{events, DriverStoreCall, HwTreeService};
    use rustos_log::{log, Event, Field, Level};
    use rustos_rt::LogSink;
    use rustos_util::fmt::{format_hex_u64, format_usize};

    /// Buffer the catalogue and each `Load` reply are received into, sized to
    /// the endpoint's `DRIVER_STORE_MAX_REPLY` so a full catalogue is never
    /// truncated. Disjoint from the tree buffer so
    /// a load can run while a tree snapshot is being decoded.
    const REPLY_BUF_LEN: usize = 64 * 1024;

    /// The stable name of a node's device class for the log line. An
    /// unknown (un-modelled) discriminant is reported as `?`, never
    /// guessed.
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
    /// emits each tree/node report through the kernel's diagnostic log via
    /// [`LogSink`] (the serial UART on a debug build) — never `stderr`. The loop's control flow is host-tested in
    /// `rustos_devmgr::service`; this is the freestanding I/O it binds.
    struct RtTreeService;

    impl HwTreeService for RtTreeService {
        fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            rustos_rt::hw_tree_read(buf).map_err(errno_from)
        }

        fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno> {
            // Block until the tree's generation advances past the one last
            // observed; `u64::MAX` is the effectively unbounded wait a
            // device manager holds for the life of the system. A negative
            // return is a `-errno` the loop fails closed on.
            let waited = rustos_rt::hw_tree_wait(last_generation, u64::MAX);
            if waited < 0 {
                return Err(errno_from(waited));
            }
            Ok(())
        }

        fn on_header(&mut self, header: &HwTreeHeader) {
            // Verbose boot/hotplug diagnostics: emitted at `Debug` so they are
            // filtered out by default and surface only when the level is
            // lowered. Routed through the kernel diagnostic
            // log, never `stderr`.
            let mut gen = [0u8; 16];
            let mut count = [0u8; 12];
            log(
                &LogSink,
                &Event {
                    level: Level::Debug,
                    id: events::TREE_OBSERVED,
                    message: "hardware tree snapshot observed",
                    fields: &[
                        Field {
                            key: "generation",
                            value: format_hex_u64(header.generation(), &mut gen),
                        },
                        Field {
                            key: "nodes",
                            value: format_usize(header.node_count() as usize, &mut count),
                        },
                    ],
                },
            );
        }

        fn on_node(&mut self, node: &HwNode) {
            let mut id = [0u8; 12];
            let mut parent = [0u8; 12];
            let mut keys = [0u8; 12];
            // The root has no parent (the all-ones sentinel); name it `root`
            // rather than render a meaningless huge number.
            let parent_str = if node.is_root() {
                "root"
            } else {
                format_usize(node.parent() as usize, &mut parent)
            };
            log(
                &LogSink,
                &Event {
                    level: Level::Debug,
                    id: events::NODE_OBSERVED,
                    message: "hardware tree node observed",
                    fields: &[
                        Field {
                            key: "id",
                            value: format_usize(node.id() as usize, &mut id),
                        },
                        Field {
                            key: "parent",
                            value: parent_str,
                        },
                        Field {
                            key: "class",
                            value: class_name(node.class()),
                        },
                        Field {
                            key: "keys",
                            value: format_usize(node.match_keys().len(), &mut keys),
                        },
                    ],
                },
            );
        }
    }

    /// The production [`DriverStoreCall`] backing: it binds the `ipc_call`
    /// `abi-v1` syscall to the read-only `/System` driver-store endpoint
    /// ([`DRIVER_STORE_ENDPOINT`]) the kernel store service serves. The protocol logic (request framing, reply
    /// decoding) is host-tested in `rustos_devmgr::store`; this is the
    /// freestanding I/O it binds. The kernel re-checks the
    /// caller's `CAP_DRV_LOAD` on every call; this client adds no authority.
    struct RtStoreCall;

    impl DriverStoreCall for RtStoreCall {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // `ipc_call` returns the raw `-errno` on failure; recover the
            // typed `Errno` and surface it fail-closed.
            rustos_rt::ipc_call(DRIVER_STORE_ENDPOINT, request, reply).map_err(errno_from)
        }
    }

    /// Program entry point. Runs the reactive match-and-load loop for the
    /// life of the service (`budget = None`): fetch the catalogue once, then
    /// read the discovered tree, load a driver for every matched node, and
    /// block on every generation advance to re-match. The
    /// loop returns only on a fail-closed tree-seam error; PID 1 `init`
    /// supervises and relaunches the service.
    fn main() -> i32 {
        // The catalogue/load reply buffer. The tree snapshot is read into a
        // separate, service-owned buffer that grows to fit the discovered
        // tree (`rustos_devmgr::run`) — a real board's
        // firmware tree dwarfs QEMU `virt`'s, so a fixed stack buffer here
        // would be a scaling cliff. The stack sizing
        // (`spawn_layout::USER_STACK_PAGES`, ~1.1 MiB) covers this 64 KiB
        // reply buffer comfortably.
        let mut reply_buf = [0u8; REPLY_BUF_LEN];
        match rustos_devmgr::run(
            &mut RtTreeService,
            &mut RtStoreCall,
            &LogSink,
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
