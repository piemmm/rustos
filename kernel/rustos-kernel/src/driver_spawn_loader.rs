//! Production driver-load mechanism that spawns a verified driver into its
//! own user-space process (`plans/PI.md` P10 5d-2-ii; PLAN Stage 4.HW item
//! 5).
//!
//! [`crate::driver_loader`] admits an *in-kernel* driver through the signed
//! `drvhost::Host::load` gate and completes registration with an in-process
//! `register()` call. This module is its user-space sibling: a driver whose
//! manifest is `kind = UserSpace` is admitted through the *same* signed gate
//! — Ed25519 signature against the build's trust anchor, the `CAP_DRV_LOAD`
//! gate, the syscall-table-hash match, and bind-table validation — and then
//! **spawned into its own hardware-isolated process** rather than run in the
//! kernel's domain (`AGENTS.md` §4 — drivers in user space wherever
//! feasible; §18.6 — leaf drivers live in the discovered tier).
//!
//! The crucial security property this module realises is §18.3: *a loaded
//! driver receives only the resource capabilities its matched node
//! requested.* The device manager forwards the matched hardware-tree node's
//! [`HwResource`] requests through the [`rustos_devmgr::DriverLoader`] seam;
//! [`SpawnDriverLoader::load`] threads them, unchanged, into the privileged
//! spawn, which mints the new process one unforgeable, owner-checked grant
//! per resource and nothing more (`KernelSpawnCtx`'s `grants` field — see
//! [`DriverProcessSpawn`]). The resources originate kernel-side, from the
//! kernel's own discovered hardware tree, never from an untrusted caller
//! (`AGENTS.md` §4 — no ambient authority), so spawning a driver can never
//! hand it authority over a window its node did not expose.
//!
//! # The architecture seam
//!
//! Creating a process is architecture-specific (it builds a fresh page-table
//! hierarchy and admits a kthread on the running CPU), so this module keeps
//! the load *policy* (the gate + the resource threading) architecture-neutral
//! and reaches the *mechanism* through the [`DriverProcessSpawn`] trait. A
//! concrete implementation builds a `KernelSpawnCtx` over the live kernel
//! subsystems and admits the driver through the architecture's
//! `ProcessSpawn::spawn_with` path; because that names kernel/core's
//! feature-selected concrete scheduler (which the production binary
//! deliberately never names, §17.1), the aarch64 implementation lives with
//! its consumer — the `-M virt` driver-autoload vertical — exactly as that
//! vertical already names the concrete scheduler to build the rest of the
//! kernel state. Host tests here supply a recording double, so the gate and
//! resource-threading logic are exercised on the CI host without a scheduler
//! (`AGENTS.md` §2.2).

use rustos_abi::hwtree::HwResource;
use rustos_abi::{DriverError, DriverHandle, Errno, ABI_VERSION_CURRENT};
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_devmgr::DriverLoader;
use rustos_drvhost::{
    DriverSpawner, Host, HostConfig, HostError, ImageSource, Sink, SpawnContext, SpawnRegisterError,
};
use rustos_kernel_core::{InitSpawnCtx, ProcessSpawn};
use rustos_kernel_syscall::SYSCALL_TABLE_HASH;

/// Spawn a verified user-space driver image into its own process.
///
/// The single architecture-specific step of [`SpawnDriverLoader`]: build a
/// fresh, hardware-isolated address space for `rxe`, admit it as a runnable
/// process granted exactly `granted` (the manifest∩caller capability set,
/// `AGENTS.md` §5.2) and one device-resource grant per entry of `grants`
/// (the matched node's requests, §18.3), hand it `args` as its
/// startup-argument vector, and return the new process id.
///
/// The implementation re-asserts every kernel-side check (the spawn
/// producer re-checks `CAP_PROC_SPAWN` and re-parses the `rxe` against the
/// kernel's syscall CFI tag) and mints the grants owner-checked against the
/// child's own kernel-trusted id — the host adds no authority of its own
/// (`AGENTS.md` §4 / §5.4).
pub trait DriverProcessSpawn {
    /// Spawn `rxe` as a user-space driver process.
    ///
    /// # Errors
    ///
    /// A stable [`Errno`] for every failure (`NoSpace` on resource
    /// exhaustion, `BadMagic` on an `rxe` that fails the CFI-tag re-parse,
    /// `AlreadyExists` on a registration conflict) — never a panic
    /// (`AGENTS.md` §2.9).
    ///
    /// `node_id` is the discovered hardware-tree node the driver was matched
    /// for (`AGENTS.md` §18.3); the kernel records it against the child so a
    /// later `hw_emit_node` parents the published child under exactly that
    /// node and the emitter cannot forge its tree position (`AGENTS.md`
    /// §4 / §5.4).
    fn spawn_driver(
        &self,
        rxe: &[u8],
        granted: CapabilitySet,
        grants: &[HwResource],
        args: &[&[u8]],
        node_id: Option<u32>,
    ) -> Result<u64, Errno>;
}

/// The production [`DriverProcessSpawn`]: drive a driver spawn through the
/// kernel/core [`InitSpawnCtx::spawn_driver_process`] seam.
///
/// This is the bin crate's scheduler-agnostic bridge between the autoload
/// policy ([`SpawnDriverLoader`]) and the kernel's spawn mechanism. It holds
/// the boot-time [`InitSpawnCtx`] (`rustos_kernel_core::KernelInitSpawner`,
/// which owns the live scheduler / capability table / address-space registry)
/// and the architecture's [`ProcessSpawn`] producer, and forwards each
/// `spawn_driver` straight to [`InitSpawnCtx::spawn_driver_process`]. The
/// `KernelSpawnCtx` assembly — and therefore every mention of the
/// feature-selected concrete scheduler — stays inside kernel/core, so this
/// bin-crate type names neither the scheduler nor `KernelSpawnCtx`
/// (`AGENTS.md` §17.1 / §2.2).
///
/// It adds no authority of its own: the child receives exactly the
/// gate-derived capability set and the matched node's resource grants the
/// seam mints (`AGENTS.md` §4 / §18.3).
pub struct InitCtxDriverProcessSpawn<'a> {
    /// The boot-time init-spawn context owning the live kernel registries
    /// the seam builds the child's [`KernelSpawnCtx`](rustos_kernel_core::KernelSpawnCtx)
    /// over.
    init_ctx: &'a dyn InitSpawnCtx,
    /// The architecture's process-spawn producer that builds the isolated
    /// address space and re-asserts every kernel-side check.
    producer: &'a dyn ProcessSpawn,
}

impl<'a> InitCtxDriverProcessSpawn<'a> {
    /// Bridge driver spawns to `init_ctx`'s
    /// [`spawn_driver_process`](InitSpawnCtx::spawn_driver_process), built
    /// over the architecture `producer`.
    #[must_use]
    pub fn new(init_ctx: &'a dyn InitSpawnCtx, producer: &'a dyn ProcessSpawn) -> Self {
        Self { init_ctx, producer }
    }
}

impl DriverProcessSpawn for InitCtxDriverProcessSpawn<'_> {
    fn spawn_driver(
        &self,
        rxe: &[u8],
        granted: CapabilitySet,
        grants: &[HwResource],
        args: &[&[u8]],
        node_id: Option<u32>,
    ) -> Result<u64, Errno> {
        self.init_ctx
            .spawn_driver_process(self.producer, rxe, granted, grants, args, node_id)
    }
}

/// Map a spawn-path [`Errno`] onto the [`DriverError`] the
/// [`DriverSpawner`] contract carries.
///
/// The [`Host`] surfaces a register/spawn failure to the device manager as
/// [`HostError::DriverRegisterFailed`] (→ [`Errno::NotImplemented`]); the
/// inner [`DriverError`] is what reaches the drvhost audit record, so it is
/// mapped to the nearest typed cause rather than collapsed. An unexpected
/// code maps to [`DriverError::DeviceFault`] — fail closed, never silently
/// succeed (`AGENTS.md` §2.9).
fn spawn_errno_as_driver_error(errno: Errno) -> DriverError {
    match errno {
        Errno::NoSpace => DriverError::LengthOutOfRange,
        Errno::BadMagic => DriverError::BadMagic,
        Errno::PermissionDenied => DriverError::PermissionDenied,
        Errno::NotImplemented => DriverError::NotImplemented,
        Errno::AlreadyExists => DriverError::Busy,
        _ => DriverError::DeviceFault,
    }
}

/// [`DriverSpawner`] that completes a verified image's load by spawning it
/// into its own process through the [`DriverProcessSpawn`] seam, granting it
/// the matched node's device resources.
///
/// Borrows live only for the `spawn_and_register` call (the [`Host`] holds
/// this for the duration of one `load`); nothing is retained.
struct SpawningDriverSpawner<'a> {
    spawn: &'a dyn DriverProcessSpawn,
    /// The matched hardware-tree node's resource requests (`AGENTS.md`
    /// §18.3); minted as the new process's device-resource grants.
    grants: &'a [HwResource],
    /// The startup-argument vector handed to the driver process
    /// (`rustos_rt::arg`) — e.g. the reply-endpoint id it announces
    /// readiness over.
    args: &'a [&'a [u8]],
    /// The matched hardware-tree node the driver was loaded for
    /// (`AGENTS.md` §18.3); recorded against the child so its `hw_emit_node`
    /// children are parented under it. [`None`] when the load is not
    /// node-matched.
    node_id: Option<u32>,
}

impl DriverSpawner for SpawningDriverSpawner<'_> {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError> {
        // The gate has verified the image; `ctx.payload` is the driver
        // program `rxe`, `ctx.granted` the manifest∩caller capability set.
        // Spawn it with exactly that authority plus the matched node's
        // resource grants (`AGENTS.md` §4 / §18.3) — no ambient authority,
        // no resource the node did not expose.
        let pid = self
            .spawn
            .spawn_driver(
                ctx.payload,
                ctx.granted,
                self.grants,
                self.args,
                self.node_id,
            )
            .map_err(|e| SpawnRegisterError::Register(spawn_errno_as_driver_error(e)))?;
        // The spawned process id doubles as the driver's reported handle;
        // the host mints its own unforgeable handle on success, so this is
        // informational. A zero pid is impossible from a successful admit,
        // but is rejected fail-closed rather than asserted (`AGENTS.md`
        // §2.9).
        DriverHandle::from_raw(pid)
            .map_err(|_| SpawnRegisterError::Register(DriverError::DeviceFault))
    }
}

/// Admits a discovered user-space driver through the signed
/// `drvhost::Host::load` gate and spawns it into its own process, granting
/// it the matched node's device resources (`AGENTS.md` §18.3).
///
/// Implements [`rustos_devmgr::DriverLoader`] so the device manager's
/// autoload walk drives it directly: for each bound node the manager calls
/// [`load`](DriverLoader::load) with the node's path and its
/// [`HwResource`] requests, and this loader
/// runs the full §8 gate then the privileged spawn. The §17.4 layering keeps
/// the device manager on `lib/*` only; this loader is the kernel binary's
/// integration point (the kernel binary is the one place permitted to bridge
/// `devmgr` policy to the kernel spawn mechanism).
pub struct SpawnDriverLoader<'a> {
    /// The driver-signing trust anchors the gate verifies against — the
    /// build's embedded key(s) (`AGENTS.md` §8 / §9).
    trusted: &'a [Ed25519PublicKey],
    /// Supplies the signed `.rxe` image bytes for a `/System/Drivers/` path.
    source: &'a dyn ImageSource,
    /// Audit sink every gate decision is logged through.
    sink: &'a dyn Sink,
    /// The architecture spawn mechanism (`AGENTS.md` §2.2).
    spawn: &'a dyn DriverProcessSpawn,
    /// Startup-argument vector handed to every spawned driver — e.g. the
    /// reply-endpoint id it announces readiness over.
    args: &'a [&'a [u8]],
    /// The matched hardware-tree node id the driver is loaded for
    /// (`AGENTS.md` §18.3), recorded against the spawned child so its
    /// `hw_emit_node` children are parented under it. [`None`] when the load
    /// is not node-matched.
    node_id: Option<u32>,
}

impl<'a> SpawnDriverLoader<'a> {
    /// Build a loader admitting against `trusted`, reading images from
    /// `source`, spawning through `spawn`, handing each driver `args`, and
    /// auditing to `sink`.
    #[must_use]
    pub fn new(
        trusted: &'a [Ed25519PublicKey],
        source: &'a dyn ImageSource,
        sink: &'a dyn Sink,
        spawn: &'a dyn DriverProcessSpawn,
        args: &'a [&'a [u8]],
        node_id: Option<u32>,
    ) -> Self {
        Self {
            trusted,
            source,
            sink,
            spawn,
            args,
            node_id,
        }
    }
}

impl DriverLoader for SpawnDriverLoader<'_> {
    fn load(
        &mut self,
        path: &str,
        resources: &[HwResource],
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, Errno> {
        // The matched node's resource requests become the new process's
        // device-resource grants; the gate runs first, so a refused image
        // is never spawned (`AGENTS.md` §5.4 — fail closed before any
        // state).
        let spawner = SpawningDriverSpawner {
            spawn: self.spawn,
            grants: resources,
            args: self.args,
            node_id: self.node_id,
        };
        let mut host = Host::new(HostConfig {
            trusted_signers: self.trusted,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            accepted_abi_version: ABI_VERSION_CURRENT,
            source: self.source,
            spawner: &spawner,
            sink: self.sink,
            // A spawned user-space driver maps its own register windows and
            // carves its own DMA region over the `mmio_map` / `dma_alloc`
            // syscalls against the grants minted here (`lib/drvrt`), never
            // through an in-kernel host view — so the gate ships neither
            // (`AGENTS.md` §4 / §5.4).
            virtio_host_factory: None,
            mmio_mapper: None,
        });
        host.load(path, caller_caps).map_err(HostError::as_errno)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    use rustos_abi::{CapabilityId, DriverHost, DriverKind, DriverManifest};

    /// One recorded `spawn_driver` call: the payload bytes, the granted
    /// capability set, and the node's resource grants the gate forwarded.
    type RecordedSpawn = (
        alloc::vec::Vec<u8>,
        CapabilitySet,
        alloc::vec::Vec<HwResource>,
    );

    /// Records every `spawn_driver` call so a test can assert exactly what
    /// the gate handed the spawn mechanism.
    struct RecordingSpawn {
        calls: RefCell<alloc::vec::Vec<RecordedSpawn>>,
        /// Pid to return, or `Err` to simulate a spawn failure.
        result: Result<u64, Errno>,
    }

    impl RecordingSpawn {
        fn ok(pid: u64) -> Self {
            Self {
                calls: RefCell::new(alloc::vec::Vec::new()),
                result: Ok(pid),
            }
        }

        fn failing(errno: Errno) -> Self {
            Self {
                calls: RefCell::new(alloc::vec::Vec::new()),
                result: Err(errno),
            }
        }
    }

    impl DriverProcessSpawn for RecordingSpawn {
        fn spawn_driver(
            &self,
            rxe: &[u8],
            granted: CapabilitySet,
            grants: &[HwResource],
            _args: &[&[u8]],
            _node_id: Option<u32>,
        ) -> Result<u64, Errno> {
            self.calls
                .borrow_mut()
                .push((rxe.to_vec(), granted, grants.to_vec()));
            self.result
        }
    }

    /// Minimal granted-capability view for a hand-built [`SpawnContext`].
    struct StubHost {
        granted: CapabilitySet,
    }

    impl DriverHost for StubHost {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            self.granted.contains(cap)
        }
        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }
    }

    fn stub_manifest() -> DriverManifest {
        DriverManifest {
            magic: rustos_abi::DRIVER_MANIFEST_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: 0,
            capability_count: 0,
            syscall_table_hash: [0u8; 32],
            signer_pubkey: [0u8; 32],
            signature: [0u8; 64],
        }
    }

    fn granted_set() -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        set.insert(CapabilityId::MMIO_MAP);
        set.insert(CapabilityId::MEM_DMA);
        set
    }

    #[test]
    fn spawner_threads_payload_granted_caps_and_node_resources_to_the_mechanism() {
        // §18.3: the matched node's resource requests, the verified
        // payload, and exactly the granted capability set must reach the
        // spawn mechanism unchanged.
        let spawn = RecordingSpawn::ok(0x1234);
        let window = HwResource::mmio(0xfe34_0000, 0x200);
        let dma = HwResource::dma(0x3fff_ffff, 0x1000);
        let grants = [window, dma];
        let args: [&[u8]; 2] = [b"drv", b"7"];
        let spawner = SpawningDriverSpawner {
            spawn: &spawn,
            grants: &grants,
            args: &args,
            node_id: Some(0x42),
        };
        let manifest = stub_manifest();
        let host = StubHost {
            granted: granted_set(),
        };
        let ctx = SpawnContext {
            manifest: &manifest,
            payload: b"the-driver-rxe-bytes",
            host: &host,
            granted: granted_set(),
        };
        let handle = spawner
            .spawn_and_register(&ctx)
            .expect("spawn succeeds and reports a handle");
        assert_eq!(handle.as_u64(), 0x1234);
        let calls = spawn.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, b"the-driver-rxe-bytes");
        assert_eq!(calls[0].1, granted_set());
        assert_eq!(calls[0].2, alloc::vec![window, dma]);
    }

    #[test]
    fn a_spawn_failure_is_reported_as_a_register_error_not_a_panic() {
        // Fail closed: a spawn-mechanism error becomes a typed register
        // failure the host maps to `Errno::NotImplemented`, never a panic
        // (`AGENTS.md` §2.9).
        let spawn = RecordingSpawn::failing(Errno::NoSpace);
        let spawner = SpawningDriverSpawner {
            spawn: &spawn,
            grants: &[],
            args: &[],
            node_id: None,
        };
        let manifest = stub_manifest();
        let host = StubHost {
            granted: CapabilitySet::empty(),
        };
        let ctx = SpawnContext {
            manifest: &manifest,
            payload: b"x",
            host: &host,
            granted: CapabilitySet::empty(),
        };
        let err = spawner
            .spawn_and_register(&ctx)
            .expect_err("a spawn failure must not yield a handle");
        assert_eq!(
            err,
            SpawnRegisterError::Register(DriverError::LengthOutOfRange)
        );
    }

    #[test]
    fn spawn_errno_mapping_is_total_and_fails_closed() {
        assert_eq!(
            spawn_errno_as_driver_error(Errno::BadMagic),
            DriverError::BadMagic
        );
        assert_eq!(
            spawn_errno_as_driver_error(Errno::PermissionDenied),
            DriverError::PermissionDenied
        );
        // An unexpected code never maps to a success-adjacent value.
        assert_eq!(
            spawn_errno_as_driver_error(Errno::BadAddress),
            DriverError::DeviceFault
        );
    }

    use alloc::boxed::Box;

    use rustos_drvhost::Event;
    use rustos_kernel_core::KernelStack;
    use rustos_kernel_mem::{
        BootMemoryMap, FrameAllocator, LiveUserSpace, MemoryRegion, PhysAddr, PhysMap, RegionKind,
        UserAddressSpace, PAGE_SIZE,
    };

    /// No-op audit sink for the unused [`InitSpawnCtx::audit`] accessor.
    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    /// One recorded [`InitSpawnCtx::spawn_driver_process`] call: the payload
    /// bytes, whether the forwarded capability set carried `CAP_DRV_LOAD`,
    /// the node's resource grants, and the startup-argument count.
    type RecordedDriverProcess = (
        alloc::vec::Vec<u8>,
        bool,
        alloc::vec::Vec<HwResource>,
        usize,
    );

    /// An [`InitSpawnCtx`] that records what
    /// [`spawn_driver_process`](InitSpawnCtx::spawn_driver_process) is
    /// handed and returns a fixed PID, so the host suite can prove
    /// [`InitCtxDriverProcessSpawn`] forwards its inputs to the seam
    /// unchanged. `frames`/`audit` exist only to satisfy the trait — the
    /// recorded override never consults them — and `admit_init` is
    /// unreachable for the same reason.
    struct RecordingInitCtx {
        frames: FrameAllocator,
        sink: NullSink,
        recorded: RefCell<Option<RecordedDriverProcess>>,
        pid: u64,
    }

    impl RecordingInitCtx {
        fn new(pid: u64) -> Self {
            static mut REGION: [u8; PAGE_SIZE * 4] = [0u8; PAGE_SIZE * 4];
            let mut map = BootMemoryMap::new();
            map.push(MemoryRegion {
                start: PhysAddr::new(core::ptr::addr_of!(REGION) as u64),
                length: (PAGE_SIZE * 4) as u64,
                kind: RegionKind::Usable,
            });
            Self {
                frames: FrameAllocator::new(&map).expect("one-region allocator"),
                sink: NullSink,
                recorded: RefCell::new(None),
                pid,
            }
        }
    }

    impl InitSpawnCtx for RecordingInitCtx {
        fn frames(&self) -> &FrameAllocator {
            &self.frames
        }

        fn audit(&self) -> &(dyn Sink + Sync) {
            &self.sink
        }

        unsafe fn admit_init(
            &self,
            _caps: CapabilitySet,
            _space: Box<dyn UserAddressSpace + Send + Sync>,
            _physmap: Box<dyn PhysMap + Send + Sync>,
            _stack: Box<dyn KernelStack + Send>,
            _pre_resume: Box<dyn FnMut(u64) + Send>,
            _live: Option<Box<dyn LiveUserSpace + Send>>,
            _enter: Box<dyn FnMut() + Send>,
        ) {
            unreachable!("the driver-spawn adapter drives spawn_driver_process, not admit_init")
        }

        fn spawn_driver_process(
            &self,
            _spawn: &dyn ProcessSpawn,
            rxe: &[u8],
            caps: CapabilitySet,
            grants: &[HwResource],
            args: &[&[u8]],
            _node_id: Option<u32>,
        ) -> Result<u64, Errno> {
            *self.recorded.borrow_mut() = Some((
                rxe.to_vec(),
                caps.contains(CapabilityId::DRV_LOAD),
                grants.to_vec(),
                args.len(),
            ));
            Ok(self.pid)
        }
    }

    /// A [`ProcessSpawn`] that must never be invoked: the recording
    /// `InitSpawnCtx` overrides `spawn_driver_process` and ignores the
    /// producer, so the adapter never reaches it.
    struct UnusedProcessSpawn;
    impl ProcessSpawn for UnusedProcessSpawn {
        fn spawn(
            &self,
            _program: &rustos_kernel_core::EmbeddedProgram,
            _ctx: &dyn rustos_kernel_core::SpawnCtx,
        ) -> Result<u64, Errno> {
            unreachable!("the recording context does not consult the producer")
        }
    }

    #[test]
    fn init_ctx_adapter_forwards_to_the_seam_unchanged() {
        // `InitCtxDriverProcessSpawn` must hand the verified payload, the
        // gate-derived capability set, the node's grants, and the argument
        // vector straight to `InitSpawnCtx::spawn_driver_process` and return
        // its PID — the bin crate's scheduler-agnostic bridge to the kernel
        // spawn mechanism (`AGENTS.md` §17.1 / §18.3).
        let init_ctx = RecordingInitCtx::new(0x7fff);
        let producer = UnusedProcessSpawn;
        let adapter = InitCtxDriverProcessSpawn::new(&init_ctx, &producer);

        let window = HwResource::mmio(0xfe34_0000, 0x200);
        let dma = HwResource::dma(0x3fff_ffff, 0x1000);
        let grants = [window, dma];
        let mut granted = CapabilitySet::empty();
        granted.insert(CapabilityId::DRV_LOAD);
        let args: [&[u8]; 1] = [b"reply-endpoint"];

        let pid = adapter
            .spawn_driver(b"driver-rxe", granted, &grants, &args, Some(3))
            .expect("the recording seam admits the driver");
        assert_eq!(pid, 0x7fff);

        let recorded = init_ctx.recorded.borrow();
        let (rxe_seen, had_drv_load, grants_seen, arg_count) =
            recorded.as_ref().expect("spawn_driver_process was invoked");
        assert_eq!(rxe_seen.as_slice(), b"driver-rxe");
        assert!(
            *had_drv_load,
            "the gate-derived capability set is forwarded"
        );
        assert_eq!(grants_seen.as_slice(), &[window, dma]);
        assert_eq!(*arg_count, 1);
    }
}
