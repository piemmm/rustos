//! The `Run` entry-point binary of the device-manager service, installed
//! at `/System/Services/devmgr.app/Run` — the
//! long-running user-space service PID 1 `init` launches to observe the
//! discovered hardware tree and react to it.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only,
//! so it links the Rust userland runtime `tairix-rt` — never the C ABI,
//! which exists solely for programs **not** written in Rust. `tairix-rt` provides `_start`, the per-process stack canary, the panic handler, and the syscall wrappers
//! (`hw_tree_read` / `hw_tree_wait`); `tairix_rt::entry!` names this
//! program's `main`.
//!
//! # What this service does (Design D D2b-2c)
//!
//! At startup it fetches the kernel-decoded driver **catalogue** over the
//! capability-gated `ipc_call` endpoint the kernel store service serves
//! (`tairix_abi::driver_store::DRIVER_STORE_ENDPOINT`): one entry per installed bundle, an opaque `bundle_id` plus the
//! bind table the kernel decoded from its signed manifest. The store is
//! read-only and static for the life of the system, so the catalogue is
//! fetched **once**; a fetch failure is fail-soft.
//!
//! It then reads the architecture-neutral hardware tree the kernel
//! discovered at boot through the capability-gated `hw_tree_read` syscall
//! (`CAP_SYSINFO_HW`), matches each node against
//! the catalogue with the shared [`tairix_devmatch`] policy, and asks the kernel to load the matched bundle for each winning
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
// optional `tairix-rt` runtime through the default `program` feature. The
// kernel links this crate's *library* with `default-features = false`, so
// it never builds this module (nor pulls in `tairix-rt`).
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::driver_store::{
        decode_config_reply, StoreRequest, SystemConfigFile, DRIVER_STORE_ENDPOINT,
        READ_CONFIG_REQUEST_LEN,
    };
    use tairix_abi::hwtree::HwDeviceClass;
    use tairix_abi::net_ipc::{
        NetBondConfigMsg, NetInterfaceConfigMsg, NetstackRequest, NetworkSettings, IF_NAME_LEN,
        NETSTACK_ENDPOINT,
    };
    use tairix_abi::reply::{decode_status_reply, STATUS_REPLY_LEN};
    use tairix_abi::{Errno, HwNode, HwTreeHeader};
    use tairix_devmgr::netcfg::{interface_configs_from_config, settings_from_config};
    use tairix_devmgr::{
        events, DriverStoreCall, HwTreeService, InterfaceConfigPlan, NetstackBind,
        NetworkConfigSource, NetworkInterfaceConfigSource,
    };
    use tairix_log::{log, Event, Field, Level};
    use tairix_rt::LogSink;
    use tairix_util::fmt::{format_hex_u64, format_u64, format_usize};

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

    /// The production [`HwTreeService`] backing: the reactive observe loop
    /// ([`tairix_devmgr::run`]) reads, waits, and reports through this seam,
    /// which binds the `hw_tree_read` / `hw_tree_wait` `abi-v1` syscalls and
    /// emits each tree/node report through the kernel's diagnostic log via
    /// [`LogSink`] (the serial UART on a debug build) — never `stderr`. The loop's control flow is host-tested in
    /// `tairix_devmgr::service`; this is the freestanding I/O it binds.
    struct RtTreeService;

    impl HwTreeService for RtTreeService {
        fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::hw_tree_read(buf).map_err(Errno::from_syscall)
        }

        fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno> {
            // Block until the tree's generation advances past the one last
            // observed; `u64::MAX` is the effectively unbounded wait a
            // device manager holds for the life of the system. A negative
            // return is a `-errno` the loop fails closed on.
            let waited = tairix_rt::hw_tree_wait(last_generation, u64::MAX);
            if waited < 0 {
                return Err(Errno::from_syscall(waited));
            }
            Ok(())
        }

        fn on_header(&mut self, header: &HwTreeHeader) {
            // Verbose boot/hotplug diagnostics: emitted at `Debug` so they are
            // filtered out by default and surface only when the level is
            // lowered. Routed through the kernel diagnostic
            // log, never `stderr`.
            let mut gen = [0u8; 16];
            let mut count = [0u8; 20];
            log(
                &LogSink,
                &Event {
                    level: Level::Debug,
                    id: events::TREE_OBSERVED,
                    message: "hardware tree snapshot observed",
                    fields: &[
                        Field {
                            key: "generation",
                            value: tairix_log::FieldValue::Str(format_hex_u64(
                                header.generation(),
                                &mut gen,
                            )),
                        },
                        Field {
                            key: "nodes",
                            value: tairix_log::FieldValue::Str(format_u64(
                                header.node_count(),
                                &mut count,
                            )),
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
                            value: tairix_log::FieldValue::Str(format_usize(
                                node.id() as usize,
                                &mut id,
                            )),
                        },
                        Field {
                            key: "parent",
                            value: tairix_log::FieldValue::Str(parent_str),
                        },
                        Field {
                            key: "class",
                            value: tairix_log::FieldValue::Str(class_name(node.class())),
                        },
                        Field {
                            key: "keys",
                            value: tairix_log::FieldValue::Str(format_usize(
                                node.match_keys().len(),
                                &mut keys,
                            )),
                        },
                    ],
                },
            );
        }
    }

    /// The production [`DriverStoreCall`] backing: it binds the `ipc_call`
    /// `abi-v1` syscall to the read-only `/System` driver-store endpoint
    /// ([`DRIVER_STORE_ENDPOINT`]) the kernel store service serves. The protocol logic (request framing, reply
    /// decoding) is host-tested in `tairix_devmgr::store`; this is the
    /// freestanding I/O it binds. The kernel re-checks the
    /// caller's `CAP_DRV_LOAD` on every call; this client adds no authority.
    struct RtStoreCall;

    impl DriverStoreCall for RtStoreCall {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // `ipc_call` returns the raw `-errno` on failure; recover the
            // typed `Errno` and surface it fail-closed.
            tairix_rt::ipc_call(DRIVER_STORE_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The production [`NetstackBind`] backing: hands a discovered NIC
    /// device channel to the network stack with one `ipc_call` to the
    /// reserved [`NETSTACK_ENDPOINT`] carrying a
    /// [`NetstackRequest::BindDriver`]. The kernel gates the call on the
    /// device manager's `CAP_NET_ADMIN`; this client adds no authority, and
    /// the protocol logic (which channels to bind, once each, fail-soft on
    /// refusal) is host-tested in `tairix_devmgr::netbind`.
    struct RtNetstackBind;

    impl NetstackBind for RtNetstackBind {
        fn bind_driver(
            &mut self,
            endpoint_id: u64,
            iface: &[u8; IF_NAME_LEN],
            node_location: u64,
        ) -> Result<(), Errno> {
            let request = NetstackRequest::BindDriver {
                endpoint_id,
                iface: *iface,
                node_location,
            }
            .to_le_bytes();
            let mut reply = [0u8; STATUS_REPLY_LEN];
            let len = tairix_rt::ipc_call(NETSTACK_ENDPOINT, &request, &mut reply)
                .map_err(Errno::from_syscall)?;
            decode_status_reply(&reply[..len])
        }

        fn apply_settings(&mut self, settings: NetworkSettings) -> Result<(), Errno> {
            let request = NetstackRequest::ApplyNetworkSettings(settings).to_le_bytes();
            let mut reply = [0u8; STATUS_REPLY_LEN];
            let len = tairix_rt::ipc_call(NETSTACK_ENDPOINT, &request, &mut reply)
                .map_err(Errno::from_syscall)?;
            decode_status_reply(&reply[..len])
        }

        fn apply_interface_config(&mut self, config: &NetInterfaceConfigMsg) -> Result<(), Errno> {
            let request = config.to_le_bytes();
            let mut reply = [0u8; STATUS_REPLY_LEN];
            let len = tairix_rt::ipc_call(NETSTACK_ENDPOINT, &request, &mut reply)
                .map_err(Errno::from_syscall)?;
            decode_status_reply(&reply[..len])
        }

        fn apply_bond_config(&mut self, config: &NetBondConfigMsg) -> Result<(), Errno> {
            let request = config.to_le_bytes();
            let mut reply = [0u8; STATUS_REPLY_LEN];
            let len = tairix_rt::ipc_call(NETSTACK_ENDPOINT, &request, &mut reply)
                .map_err(Errno::from_syscall)?;
            decode_status_reply(&reply[..len])
        }
    }

    /// The `ipc_call` reply buffer a config read frames into: the reply header
    /// plus the larger of the two config engines' document ceilings, so
    /// either whitelisted file fits in one bounded read. A document longer
    /// than its engine's ceiling could never parse anyway, so the buffer is
    /// sized to that ceiling, never a scalable capacity.
    const STORE_CONFIG_REPLY_LEN: usize =
        8 + if tairix_sysconfig::MAX_CONFIG_LEN > tairix_netconfig::MAX_CONFIG_LEN {
            tairix_sysconfig::MAX_CONFIG_LEN
        } else {
            tairix_netconfig::MAX_CONFIG_LEN
        };

    /// Read one whitelisted `/System/Settings/` config file over the
    /// always-mounted read-only `/System` **store** endpoint, copying its
    /// bytes into `out` and returning their length.
    ///
    /// The device manager configures interfaces on the same read-only
    /// `/System` volume the drivers autoload from, and must do so *before*
    /// the encrypted root is unlocked — while the general VFS path
    /// (`fs_open`) is not yet mounted. The store service already owns that
    /// volume, so the config is read through it (`StoreRequest::ReadConfig`)
    /// rather than the VFS, over the same `CAP_DRV_LOAD`-gated endpoint the
    /// catalogue uses; the kernel re-checks the capability, so this adds no
    /// authority. [`None`] on any failure (an absent file is a benign
    /// in-band `NotFound`, an oversized or corrupt frame, a transport
    /// error), so a caller keeps its safe defaults and retries on the next
    /// generation bump, never guessing (fail closed).
    fn read_store_config(which: SystemConfigFile, out: &mut [u8]) -> Option<usize> {
        let mut request = [0u8; READ_CONFIG_REQUEST_LEN];
        let n = StoreRequest::ReadConfig { which }
            .encode(&mut request)
            .ok()?;
        // Deliberately on the stack: this runs before the root unlock, so the
        // heap's producer is not up yet. The spawn stack sizing covers it.
        #[allow(clippy::large_stack_arrays)]
        let mut reply = [0u8; STORE_CONFIG_REPLY_LEN];
        let len = tairix_rt::ipc_call(DRIVER_STORE_ENDPOINT, &request[..n], &mut reply).ok()?;
        let bytes = decode_config_reply(&reply[..len]).ok()?;
        if bytes.len() > out.len() {
            return None;
        }
        out[..bytes.len()].copy_from_slice(bytes);
        Some(bytes.len())
    }

    /// The production [`NetworkConfigSource`] backing: reads the stack-wide
    /// `net.*` policy from `/System/Settings/Configuration/system.conf` over
    /// the read-only `/System` store endpoint ([`read_store_config`]) and
    /// maps it through the one shared `lib/sysconfig` engine
    /// ([`settings_from_config`]).
    ///
    /// The read is over the store endpoint (not the VFS) so it works before
    /// the root unlock, and is `CAP_DRV_LOAD`-gated by the kernel; this seam
    /// adds no authority. It returns [`None`] on any failure — the store not
    /// yet reachable, an absent (`NotFound`), unreadable, or oversized
    /// document, or one the engine cannot parse — so delivery keeps the
    /// network stack on its safe defaults and retries on the next generation
    /// bump, never guessing at a policy (fail closed).
    struct RtNetworkConfig;

    impl NetworkConfigSource for RtNetworkConfig {
        fn load(&mut self) -> Option<NetworkSettings> {
            // One bounded read suffices: the engine refuses any document
            // longer than its own ceiling, so a store that does not fit this
            // buffer could never parse anyway.
            let mut buf = [0u8; tairix_sysconfig::MAX_CONFIG_LEN];
            let len = read_store_config(SystemConfigFile::System, &mut buf)?;
            let text = core::str::from_utf8(&buf[..len]).ok()?;
            let config = tairix_sysconfig::SystemConfig::parse(text).ok()?;
            Some(settings_from_config(&config))
        }
    }

    /// The production [`NetworkInterfaceConfigSource`] backing: reads the
    /// per-interface configuration from
    /// `/System/Settings/Network/network.conf` over the read-only `/System`
    /// store endpoint ([`read_store_config`]) and maps it through the one
    /// shared `lib/netconfig` engine
    /// ([`interface_configs_from_config`](tairix_devmgr::netcfg::interface_configs_from_config)).
    ///
    /// The read is over the store endpoint (not the VFS) so it works before
    /// the root unlock — the device manager binds interfaces on the same
    /// read-only volume the drivers autoload from — and is `CAP_DRV_LOAD`-
    /// gated by the kernel; this seam adds no authority. It returns [`None`]
    /// on any failure — the store not yet reachable, an absent (`NotFound`),
    /// unreadable, or oversized document, or one the engine cannot parse or
    /// validate — so delivery leaves the interfaces unconfigured and retries
    /// on the next generation bump, never guessing at a partial configuration
    /// (fail closed).
    struct RtNetworkInterfaceConfig;

    impl NetworkInterfaceConfigSource for RtNetworkInterfaceConfig {
        fn load(&mut self) -> Option<InterfaceConfigPlan> {
            let mut buf = [0u8; tairix_netconfig::MAX_CONFIG_LEN];
            let len = read_store_config(SystemConfigFile::Network, &mut buf)?;
            let text = core::str::from_utf8(&buf[..len]).ok()?;
            // `parse` validates the whole document (including the bond and
            // static-addressing invariants), so a semantically inconsistent
            // store is refused whole here rather than delivered as a partial
            // guess (fail closed).
            let config = tairix_netconfig::NetworkConfig::parse(text).ok()?;
            Some(interface_configs_from_config(&config))
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
        // tree (`tairix_devmgr::run`) — a real board's
        // firmware tree dwarfs QEMU `virt`'s, so a fixed stack buffer here
        // would be a scaling cliff. The stack sizing
        // (`spawn_layout::USER_STACK_PAGES`, ~1.1 MiB) covers this 64 KiB
        // reply buffer comfortably.
        #[allow(clippy::large_stack_arrays)]
        let mut reply_buf = [0u8; REPLY_BUF_LEN];
        match tairix_devmgr::run(
            &mut RtTreeService,
            &mut RtStoreCall,
            &mut RtNetstackBind,
            &mut RtNetworkConfig,
            &mut RtNetworkInterfaceConfig,
            &LogSink,
            &mut reply_buf,
            None,
        ) {
            Ok(()) => 0,
            Err(err) => {
                // Fail loud: state the reason for the abnormal exit through
                // the kernel diagnostic log (this service has no terminal
                // consumer), carrying the errno so the refusing seam is
                // diagnosable, then exit non-zero for `init`'s supervision.
                let mut code = [0u8; 12];
                let errno = usize::try_from(err.as_i32()).unwrap_or(0);
                log(
                    &LogSink,
                    &Event {
                        level: Level::Error,
                        id: events::TREE_SEAM_FAILED,
                        message: "hardware-tree seam failed; devmgr exiting for supervision",
                        fields: &[Field {
                            key: "errno",
                            value: tairix_log::FieldValue::Str(format_usize(errno, &mut code)),
                        }],
                    },
                );
                1
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `tairix-rt` `_start` path is not compiled
// — on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
