//! Boot-installed bundle of the process-launch services the asynchronous
//! launch path's child loading body captures (`plans/FIX-DESKTOP.md`
//! §2.6.5, item 2).
//!
//! # Why this bundle exists
//!
//! The non-blocking desktop fix defers a spawned child's image load off
//! the calling task and onto the **child's own** first scheduled slice.
//! The child runs a loading body — `wait_app_store` → `load_store_bundle`
//! → derive-and-install its effective capability set → build its image →
//! [`crate::kthread::Yielder::become_user`] → enter user mode — on its own
//! kernel stack, long after the `spawn` syscall handler has returned. That
//! body therefore cannot borrow the handler's short-lived `KernelState`
//! borrows; it captures a single `&'static` handle to the load services
//! instead.
//!
//! [`SpawnServices`] is that handle: one read-only, set-once, `'static`
//! struct carrying every service the loading body touches. Production
//! backs each field from the boot path's `Box::leak`'d `KernelState`; host
//! tests install a leaked fixture. This mirrors the established
//! set-once boot-handle idiom ([`crate::dispatch_slot`],
//! [`crate::devres::install_shared_mem_facility`]): a
//! [`tairix_sync::OnceCell`]-guarded read-only handle, **not** a mutable
//! global — the charter's "no global mutable static" rule holds.
//!
//! # Non-generic on purpose
//!
//! The loading body's only architecture-generic dependencies are three
//! scalar operations — the CPU it parks on, the monotonic tick it stamps
//! the child's capability record with, and the monotonic nanoseconds the
//! bundle read's clock uses. Those three are type-erased behind the tiny
//! non-generic [`SpawnRuntime`] trait object (a generic
//! [`ArchSpawnRuntime`] leaked at boot over the arch handle). Every other
//! service the body touches is already non-generic, so [`SpawnServices`]
//! is a plain struct in a plain [`tairix_sync::OnceCell`] — sidestepping
//! Rust's ban on generic statics without making the whole launch path
//! generic.
//!
//! # Reserved contract
//!
//! This is the reserved contract the deferred-load flip
//! (`plans/FIX-DESKTOP.md` DESK-1) builds on, landed ahead of the wiring
//! exactly as the task-model primitive ([`crate::kthread::Yielder::become_user`])
//! and the reserved `LOAD_*` exit-status ABI were. The bundle is complete
//! and independently tested; the live `spawn` handler and the child
//! loading body read it when the flip lands.

use tairix_kernel_mem::FrameAllocator;
use tairix_kernel_sched_api::{CpuId, SchedulerArch};
use tairix_kernel_sec::CapTable;
use tairix_log::Sink;
use tairix_sync::{OnceCell, RwLock};

use crate::appspawn::AppStore;
use crate::aspace::AddressSpaceRegistry;
use crate::bootinfo::KernelArch;
use crate::fs::FilesystemService;
use crate::procwait::ProcessWait;
use crate::spawn::ArchImageBuilder;

/// The three architecture-dependent scalar operations the child loading
/// body needs, type-erased so [`SpawnServices`] stays non-generic.
///
/// The loading body is otherwise arch-neutral; these are the only values
/// it must read from the concrete architecture port:
///
/// * [`Self::current_cpu`] — the CPU the child parks on while it waits for
///   the app store latch and its bundle I/O (it parks off the run queue on
///   the CPU it currently occupies).
/// * [`Self::ticks_now`] — the kernel monotonic tick stamped onto the
///   child's capability record as its start time.
/// * [`Self::now_ns`] — the monotonic nanoseconds the bundle-read clock
///   (`AppLoader`'s signature-validity check) reads.
///
/// Implementors must never panic; the production port
/// ([`ArchSpawnRuntime`]) forwards to the arch HAL, which is total.
pub trait SpawnRuntime: Sync {
    /// The calling CPU's identifier (read live on every call, never
    /// cached — a parked task may be work-stolen to another core).
    fn current_cpu(&self) -> CpuId;

    /// The current kernel monotonic tick.
    fn ticks_now(&self) -> u64;

    /// The current monotonic time in nanoseconds on the calling CPU.
    fn now_ns(&self) -> u64;
}

/// The production [`SpawnRuntime`] over a `'static` architecture handle.
///
/// Forwards each operation to the arch HAL: [`SchedulerArch::current_cpu`]
/// / [`SchedulerArch::ticks_now`] and [`KernelArch::monotonic_ns`] on the
/// live CPU. Leaked to `'static` at boot over the `KernelState` arch
/// handle, then wired into [`SpawnServices::new`] as the runtime.
pub struct ArchSpawnRuntime<A>
where
    A: KernelArch + 'static,
{
    arch: &'static A,
}

impl<A> ArchSpawnRuntime<A>
where
    A: KernelArch + 'static,
{
    /// Bind the production runtime to the `'static` architecture handle.
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> SpawnRuntime for ArchSpawnRuntime<A>
where
    A: KernelArch + 'static,
{
    fn current_cpu(&self) -> CpuId {
        SchedulerArch::current_cpu(self.arch)
    }

    fn ticks_now(&self) -> u64 {
        SchedulerArch::ticks_now(self.arch)
    }

    fn now_ns(&self) -> u64 {
        // The live CPU is read on every call: a parked loading task can be
        // woken and re-dispatched onto a different core, so the CPU a
        // monotonic read is keyed to must be the one the task occupies now.
        self.arch
            .monotonic_ns(SchedulerArch::current_cpu(self.arch))
    }
}

/// The `'static`, set-once bundle of process-launch services the child
/// loading body captures (see the module docs).
///
/// Every field is a `'static` handle onto a live kernel subsystem. The
/// struct is non-generic and [`Sync`] — each field is itself `Sync`
/// (`&'static` to a `Sync` allocator / registry lock / audit sink / trait
/// object) — so it installs into a plain [`tairix_sync::OnceCell`] and is
/// read soundly from any CPU.
///
/// Construct it once at boot with [`Self::new`] and publish it through
/// [`install_spawn_services`]; the loading body reads it through
/// [`installed_spawn_services`].
pub struct SpawnServices {
    frames: &'static FrameAllocator,
    page_table_frames: Option<&'static FrameAllocator>,
    audit: &'static (dyn Sink + Sync),
    filesystem: &'static (dyn FilesystemService + 'static),
    app_store: Option<&'static AppStore>,
    aspaces: &'static RwLock<AddressSpaceRegistry>,
    caps: &'static RwLock<CapTable>,
    process_wait: &'static (dyn ProcessWait + 'static),
    image_builder: &'static (dyn ArchImageBuilder + 'static),
    runtime: &'static (dyn SpawnRuntime + 'static),
}

impl SpawnServices {
    /// Bundle the launch services into one `'static` handle.
    ///
    /// * `frames` — the kernel's live physical-frame allocator (the source
    ///   of the child's image and page-table frames).
    /// * `page_table_frames` — the same allocator as a `'static` borrow the
    ///   arch image builder builds the child's page tables from, or `None`
    ///   on a build that wired no `'static` allocator (the build then fails
    ///   closed).
    /// * `audit` — the audit sink the loading body records a load refusal
    ///   through, attributed to the child.
    /// * `filesystem` — the secured VFS the child's bundle read is
    ///   authorised and served through, under the child's own credential.
    /// * `app_store` — the on-disk application store a `<Name>.app/Run`
    ///   path resolves against, or `None` on a build with no store (a
    ///   store-bundle load then fails closed).
    /// * `aspaces` — the address-space registry the child registers its
    ///   frozen space and per-task state in.
    /// * `caps` — the capability table the child's loading record lives in
    ///   and whose empty set the body replaces with the derived effective
    ///   set before the child enters user mode.
    /// * `process_wait` — the parent/child wait producer.
    /// * `image_builder` — the architecture image builder that produces the
    ///   child's isolated address space from the verified `rxe`.
    /// * `runtime` — the type-erased [`SpawnRuntime`] the body reads the
    ///   live CPU / tick / nanoseconds from.
    // Mirrors `KernelSpawnCtx::new`: the same distinct kernel-state handles
    // threaded explicitly, not a one-use wrapper type.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        frames: &'static FrameAllocator,
        page_table_frames: Option<&'static FrameAllocator>,
        audit: &'static (dyn Sink + Sync),
        filesystem: &'static (dyn FilesystemService + 'static),
        app_store: Option<&'static AppStore>,
        aspaces: &'static RwLock<AddressSpaceRegistry>,
        caps: &'static RwLock<CapTable>,
        process_wait: &'static (dyn ProcessWait + 'static),
        image_builder: &'static (dyn ArchImageBuilder + 'static),
        runtime: &'static (dyn SpawnRuntime + 'static),
    ) -> Self {
        Self {
            frames,
            page_table_frames,
            audit,
            filesystem,
            app_store,
            aspaces,
            caps,
            process_wait,
            image_builder,
            runtime,
        }
    }

    /// The kernel's live physical-frame allocator.
    #[must_use]
    pub fn frames(&self) -> &'static FrameAllocator {
        self.frames
    }

    /// The `'static` page-table frame source, or `None` on a build that
    /// wired no `'static` allocator (the build fails closed).
    #[must_use]
    pub fn page_table_frames(&self) -> Option<&'static FrameAllocator> {
        self.page_table_frames
    }

    /// The audit sink the loading body records a load refusal through.
    #[must_use]
    pub fn audit(&self) -> &'static (dyn Sink + Sync) {
        self.audit
    }

    /// The secured VFS the child's bundle read is authorised through.
    #[must_use]
    pub fn filesystem(&self) -> &'static (dyn FilesystemService + 'static) {
        self.filesystem
    }

    /// The on-disk application store, or `None` on a store-less build (a
    /// store-bundle load fails closed).
    #[must_use]
    pub fn app_store(&self) -> Option<&'static AppStore> {
        self.app_store
    }

    /// The address-space registry the child registers its space in.
    #[must_use]
    pub fn aspaces(&self) -> &'static RwLock<AddressSpaceRegistry> {
        self.aspaces
    }

    /// The capability table holding the child's loading record.
    #[must_use]
    pub fn caps(&self) -> &'static RwLock<CapTable> {
        self.caps
    }

    /// The parent/child wait producer.
    #[must_use]
    pub fn process_wait(&self) -> &'static (dyn ProcessWait + 'static) {
        self.process_wait
    }

    /// The architecture image builder.
    #[must_use]
    pub fn image_builder(&self) -> &'static (dyn ArchImageBuilder + 'static) {
        self.image_builder
    }

    /// The type-erased architecture runtime (CPU / tick / nanoseconds).
    #[must_use]
    pub fn runtime(&self) -> &'static (dyn SpawnRuntime + 'static) {
        self.runtime
    }
}

/// The boot-installed launch-services bundle, published exactly once.
///
/// Set-once with the right cross-CPU ordering: [`OnceCell::set`] is a
/// release store and [`OnceCell::get`] an acquire load, so the loading
/// body on any CPU observes the fully-constructed bundle from the moment
/// [`install_spawn_services`] returns.
static INSTALLED_SPAWN_SERVICES: OnceCell<&'static SpawnServices> = OnceCell::new();

/// Publish the boot-built launch-services bundle.
///
/// Called by the boot path exactly once, after every service the bundle
/// carries is constructed and `Box::leak`'d. The bundle is set-once per
/// boot; a second publish is a programmer error surfaced as
/// [`SpawnServicesAlreadyInstalled`] rather than silently overwriting the
/// live bundle (fail loud, never a silent retry).
///
/// # Errors
///
/// Returns [`SpawnServicesAlreadyInstalled`] if a bundle was already
/// published.
pub fn install_spawn_services(
    services: &'static SpawnServices,
) -> Result<(), SpawnServicesAlreadyInstalled> {
    INSTALLED_SPAWN_SERVICES
        .set(services)
        .map_err(|_| SpawnServicesAlreadyInstalled)
}

/// The installed launch-services bundle, or `None` before boot publishes
/// one.
///
/// The child loading body reads it here and fails the load closed if it is
/// still absent (a launch before the boot path finished wiring the launch
/// path, which correct boot ordering makes unreachable but which is still
/// handled without a panic).
#[must_use]
pub fn installed_spawn_services() -> Option<&'static SpawnServices> {
    // Fold the still-empty (`Ok(None)`) and structurally-impossible
    // poisoned (`Err`) cases into one fail-closed `None` so callers see a
    // single branch (no `unwrap`/`expect`).
    match INSTALLED_SPAWN_SERVICES.get() {
        Ok(Some(services)) => Some(*services),
        Ok(None) | Err(_) => None,
    }
}

/// [`install_spawn_services`] rejected a second publish.
///
/// The bundle is set-once per boot; a second call indicates a programmer
/// error (double boot wiring). The boot path surfaces it and halts rather
/// than recovering silently.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SpawnServicesAlreadyInstalled;

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::boxed::Box;

    use tairix_abi::Errno;
    use tairix_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};

    use crate::kthread::{BoxStack, KernelStack};
    use crate::procwait::NULL_PROCESS_WAIT;
    use crate::spawn::{BuiltImage, ImageBuildCtx};
    use crate::test_sink::TestSink;

    /// A [`SpawnRuntime`] returning fixed, distinguishable values so a test
    /// can assert the bundle forwards to the runtime it was built with.
    struct FixtureRuntime {
        cpu: CpuId,
        ticks: u64,
        ns: u64,
    }

    impl SpawnRuntime for FixtureRuntime {
        fn current_cpu(&self) -> CpuId {
            self.cpu
        }

        fn ticks_now(&self) -> u64 {
            self.ticks
        }

        fn now_ns(&self) -> u64 {
            self.ns
        }
    }

    /// A minimal [`ArchImageBuilder`] stub. The reserved-contract bundle
    /// only *holds* the builder; a test never drives a build through it, so
    /// `build` fails closed and `alloc_kernel_stack` hands back a plain
    /// heap stack with no guard page to unmap.
    struct StubImageBuilder;

    impl ArchImageBuilder for StubImageBuilder {
        fn alloc_kernel_stack(
            &self,
            _frames: &FrameAllocator,
            _pt_frames: Option<&'static FrameAllocator>,
        ) -> (Box<dyn KernelStack + Send>, Option<u64>) {
            (Box::new(BoxStack::new()), None)
        }

        fn build(
            &self,
            _rxe: &[u8],
            _ctx: &dyn ImageBuildCtx,
            _args: &[&[u8]],
            _env: &[&[u8]],
        ) -> Result<BuiltImage, Errno> {
            Err(Errno::NotImplemented)
        }
    }

    /// Leak a frame allocator over a small simulated RAM window.
    fn leak_frames() -> &'static FrameAllocator {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new((PAGE_SIZE * 16) as u64),
            length: (64 * PAGE_SIZE) as u64,
        });
        Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")))
    }

    /// Build a complete, real [`SpawnServices`] from leaked fixtures.
    fn leak_services(runtime: &'static (dyn SpawnRuntime + 'static)) -> &'static SpawnServices {
        let frames = leak_frames();
        let audit: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let aspaces: &'static RwLock<AddressSpaceRegistry> =
            Box::leak(Box::new(RwLock::new(AddressSpaceRegistry::new())));
        let caps: &'static RwLock<CapTable> = Box::leak(Box::new(RwLock::new(CapTable::new())));
        let builder: &'static StubImageBuilder = Box::leak(Box::new(StubImageBuilder));
        Box::leak(Box::new(SpawnServices::new(
            frames,
            Some(frames),
            audit,
            &crate::fs::NULL_FILESYSTEM,
            None,
            aspaces,
            caps,
            &NULL_PROCESS_WAIT,
            builder,
            runtime,
        )))
    }

    #[test]
    fn getters_return_the_handles_the_bundle_was_built_with() {
        let runtime: &'static FixtureRuntime = Box::leak(Box::new(FixtureRuntime {
            cpu: 3,
            ticks: 42,
            ns: 9_000,
        }));
        let services = leak_services(runtime);

        // The scalar runtime forwards to the exact fixture it was built
        // with.
        assert_eq!(services.runtime().current_cpu(), 3);
        assert_eq!(services.runtime().ticks_now(), 42);
        assert_eq!(services.runtime().now_ns(), 9_000);

        // The `Option` handles reflect the construction choices.
        assert!(services.page_table_frames().is_some());
        assert!(services.app_store().is_none());

        // The frame allocator is the live one (it has free frames).
        assert!(services.frames().free_frames() > 0);
    }

    #[test]
    fn arch_runtime_forwards_to_the_arch_handle() {
        use crate::test_arch::TestArch;

        // `TestArch::current_cpu` reports the CPU the arch handle was built
        // for; the production runtime must forward to it unchanged.
        let arch: &'static TestArch = Box::leak(Box::new(TestArch::with_cpus(1)));
        let runtime: &'static ArchSpawnRuntime<TestArch> =
            Box::leak(Box::new(ArchSpawnRuntime::new(arch)));
        assert_eq!(
            runtime.current_cpu(),
            SchedulerArch::current_cpu(arch),
            "the production runtime forwards current_cpu to the arch handle",
        );
        // `now_ns` and `ticks_now` are total on the arch HAL — calling them
        // must not panic and must agree with the arch handle.
        assert_eq!(runtime.ticks_now(), SchedulerArch::ticks_now(arch));
        let _ = runtime.now_ns();
    }

    #[test]
    fn install_is_set_once_and_get_returns_the_installed_bundle() {
        // The module static is process-global; this test owns the single
        // install in the crate's test binary so it can assert the
        // set-once contract end to end.
        let runtime: &'static FixtureRuntime = Box::leak(Box::new(FixtureRuntime {
            cpu: 1,
            ticks: 7,
            ns: 700,
        }));
        let first = leak_services(runtime);

        // Empty before any install.
        assert!(installed_spawn_services().is_none());

        install_spawn_services(first).expect("first install succeeds");
        let got = installed_spawn_services().expect("bundle visible after install");
        assert_eq!(got.runtime().ticks_now(), 7);

        // A second publish is refused; the first bundle stays installed.
        let second = leak_services(Box::leak(Box::new(FixtureRuntime {
            cpu: 2,
            ticks: 99,
            ns: 9_900,
        })));
        assert_eq!(
            install_spawn_services(second),
            Err(SpawnServicesAlreadyInstalled),
        );
        assert_eq!(
            installed_spawn_services()
                .expect("still installed")
                .runtime()
                .ticks_now(),
            7,
            "the first bundle is never overwritten",
        );
    }
}
