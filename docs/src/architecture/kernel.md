# Kernel entry, init order, and panic policy

This page documents the architecture-neutral kernel core (`kernel/core`),
delivered by Stage 2.6 of [`PLAN.md`](../../../PLAN.md). The crate ships
**three** things and nothing else:

1. The hand-off type `BootInfo` and the `KernelArch` trait an
   architecture port (Stage 3) implements.
2. The single public entry point `kernel_main` that orchestrates
   subsystem init in a fixed, documented order.
3. The panic helper `handle_panic` that an arch port's
   `#[panic_handler]` delegates to.

Everything else (page tables, IPI plumbing, syscall registration, …)
lives elsewhere. `kernel/core` is the contract those layers meet at.

## Entry contract

The arch port's boot stub (Stage 3) is responsible for:

* zeroing BSS,
* setting up an initial stack,
* parsing the platform's boot protocol (multiboot2 / UEFI / DTB /
  `wasm-bindgen`) into a typed `BootMemoryMap` and `IdentityTableBuilder`,
* constructing a static `log_sink` and `audit_sink`,
* building an `Arc<A: KernelArch>`,
* calling `rustos_kernel_core::kernel_main(boot)`.

`kernel_main` consumes the `BootInfo`, drives the init phases, and
either parks the boot CPU via `KernelArch::halt` on success (Stage 2.7
will replace the trailing halt with the scheduler dispatch loop) or
parks it via `KernelArch::halt` on failure. **The kernel never silently
resets** — that bottom-typed return is the contract (`AGENTS.md` §2).

## Init order

`kernel_main` runs the following phases in this exact order. The order
is the audit contract with external log consumers — re-ordering would
break the boot-timeline they key off (`AGENTS.md` §5.4, §2.4).

| # | Phase   | Subsystem constructed                                                                 |
|---|---------|---------------------------------------------------------------------------------------|
| 0 | —       | `BootStarted` event emitted; `BootInfo::validate` runs.                               |
| 1 | `log`   | `rustos_log::set_max_level(boot.log_level)`.                                          |
| 2 | `mem`   | `rustos_kernel_mem::FrameAllocator::new(&boot.memory_map)`.                           |
| 3 | `sec`   | `boot.identity.verify(boot.audit_sink)` → `IdentityTable`.                            |
| 4 | `sched` | `crate::sched::Scheduler::new(boot.scheduler_config, Arc::clone(&boot.arch))` — the build-time-selected policy (§17.1). |
| 5 | `irq`   | `arch.irq_routing()` returns the architecture-installed `IrqRouting`; the kernel core builds `rustos_kernel_irq::IrqTable::new(routing.max_line)` and stores `routing.controller` in `KernelState`. Immediately after the leak, `arch.install_irq_dispatch(&state.irq)` publishes the `'static` table reference into the arch port's external-IRQ dispatcher slot (Stage 4.D Item 2-tail.2). |
| 6 | `syscall` | Production `DispatchHook` published into `boot.dispatcher_callback_slot` (see [Syscall registration phase](#syscall-registration-phase)). |
| 7 | `ipc`   | The named-port `PortRegistry` is composed into `KernelState` (`ipc: RwLock<PortRegistry>`) above and borrowed by the dispatch hook; it boots empty and the phase event fires for timeline uniformity. |
| ∞ | —       | `BootCompleted` event emitted; `arch.halt()` parks the CPU.                           |

Each phase emits exactly:

* one `KERNEL_PHASE_STARTED` record (`EventId(4001)`) with a
  `phase = <name>` field, then
* on success: one `KERNEL_PHASE_READY` record (`EventId(4002)`), or
* on failure: one `KERNEL_PHASE_FAILED` record (`EventId(4003)`)
  carrying both `phase` and `cause` fields, then `arch.halt()`.

The set of stable `cause` strings is enumerated in
`kernel/core/src/init.rs::InitError::cause`.

## Syscall registration phase

Stage 2.7 follow-up (f4) wires the production syscall dispatcher into
`kernel_main`. The phase is the kernel-side publication point for the
callback the arch-port installed before `syscall` was enabled
(`set_dispatch_callback`, `AGENTS.md` §5.4.5 — fail-closed ordering).

```text
            kernel/core                            kernel/rustos-kernel (bin)
            ────────────────────────────────────────────────────────────────────────
  sched ready                                       static DISPATCH_SLOT:
   │                                                DispatchCallbackSlot = new();
   ▼                                                   ▲
  KernelDispatchHook::new(&sched,&caps,arch,audit,&irq,ctl,&ipc,&aspaces,&rng,console) │ (from BootInfo)
  Box::leak(hook)                                       │
  slot.install_dispatcher(hook)  ──publishes──────────┘ ───> production_dispatch (f5)
   │                                                   reads slot.get(),
   ▼                                                   forwards every syscall.
  syscall ready  →  ipc started
```

The `Phase::Syscall` step:

1. Builds a `KernelDispatchHook<A>` around `KernelState`'s scheduler,
   capability table, arch port, audit sink, IRQ table/controller,
   named-port registry (`ipc`), per-task address-space registry
   (`aspaces`), the kernel random output reserve (`rng`), and the
   system `console` device (`BootInfo::console`, the discovered
   framebuffer or first UART that backs a process's standard output
   stream, written through the `stream_write` syscall, `AGENTS.md` §20;
   defaults to the fail-closed `NULL_CONSOLE`). The
   `aspaces` registry lets a handler resolve `caller.task_id` to the
   user address space + `PhysMap` the user-memory copy path walks
   (`KernelSyscallHandlers::with_caller_aspace`, `AGENTS.md` §5.4)
   without the decoupled dispatcher gaining a `kernel/mem` dependency
   (§17.4); the `rng` reserve (`rustos_rng::OutputReserve` behind a
   `RandomReserve` trait object) backs `random_get` (`AGENTS.md` §22),
   booting unseeded so a draw fails closed with `EntropyNotReady` until
   the platform-RNG entropy seam (§17.2) re-seeds it.
2. Lifts both the `KernelState` and the hook to `'static` lifetimes
   via `Box::leak` (one-shot publish; the kernel never returns from
   `kernel_main`'s halt). The leak is immutable after publish — not
   a global mutable static; every interior field carries its own
   synchronisation primitive (`Scheduler::tasks`, `RwLock<CapTable>`,
   `RwLock<PortRegistry>`, `RwLock<AddressSpaceRegistry>`,
   `RwLock<Box<dyn RandomReserve>>`).
3. Calls `boot.dispatcher_callback_slot.install_dispatcher(&hook)`.
4. On `Err(AlreadyInstalledError)` (slot already published; programmer
   error), surfaces `InitError::DispatcherAlreadyInstalled`, which the
   standard `PhaseFailed` audit-record path reports under
   `phase = "syscall"`, `cause = "syscall_dispatcher_already_installed"`,
   and halts — no silent recovery.

The arch-level `set_dispatch_callback` is **still** invoked before
`syscall` is enabled on any CPU; this phase is the *kernel-side*
publication point for the hook the eventual production callback (f5)
will read from the slot, not the trampoline.

The slot itself is `pub static DISPATCH_SLOT: DispatchCallbackSlot`
in `kernel/rustos-kernel/src/x86_64/dispatch.rs`: a normal `static` (not
`static mut`) whose set-once publication is protected by an internal
[`OnceCell`](./sync.md). The QEMU integration test
`rustos-test-kernel-arch-boot` reuses the same slot through
`rustos_kernel::boot`.

## Panic policy

The arch port owns the `#[panic_handler]` attribute (host-test builds
cannot define one because `std` already does). It is a one-liner that
delegates to `handle_panic`:

```rust,ignore
#[panic_handler]
fn rustos_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    rustos_kernel_core::handle_panic(info, &PANIC_CTX)
}
```

`PANIC_CTX: PanicContext` is stored in the per-CPU bootstrap area the
arch port owns. Building it once at boot and never mutating it is the
only global-mutable-state exception called out by `AGENTS.md` §2; the
arch port documents it there.

`handle_panic` emits exactly one `KERNEL_PANIC` record (`EventId(4010)`,
`Level::Error`) with the fields below, then calls `KernelArch::halt`.

| Key      | Value                                                |
|----------|------------------------------------------------------|
| `cpu`    | Decimal `KernelArch::current_cpu()`.                 |
| `file`   | `info.location().file()` or `"<unknown>"`.           |
| `line`   | Decimal `info.location().line()` or `"0"`.           |
| `column` | Decimal `info.location().column()` or `"0"`.         |

The handler performs **no allocation**: every formatting buffer is
stack-resident, so the panic path survives a wedged heap.

## `BootInfo` schema

`BootInfo<'a, A: KernelArch>` is `pub`-fielded and consumed by value:

| Field              | Type                              | SAFETY-INVARIANT                              |
|--------------------|-----------------------------------|-----------------------------------------------|
| `boot_cpu`         | `CpuId` (`u32`)                   | `== arch.current_cpu()` at entry.             |
| `cpu_count`        | `u32`                             | `>= 1`, `boot_cpu < cpu_count`.               |
| `command_line`     | `&'a str`                         | `len() <= MAX_COMMAND_LINE_BYTES`.            |
| `memory_map`       | `BootMemoryMap`                   | Usable regions are firmware-released RAM.     |
| `identity`         | `IdentityTableBuilder`            | Verified during the `sec` phase.              |
| `scheduler_config` | `SchedulerConfig`                 | `.cpus == cpu_count`.                         |
| `arch`             | `Arc<A>`                          | Pinned for the lifetime of the running kernel.|
| `log_sink`         | `&'static (dyn Sink + Sync)`      | Lives until power-off.                        |
| `audit_sink`       | `&'static (dyn Sink + Sync)`      | Lives until power-off.                        |
| `log_level`        | `rustos_log::Level`               | Installed before the first `PhaseStarted`.    |
| `dispatcher_callback_slot` | `&'static DispatchCallbackSlot`   | Bin-crate-owned slot; receives the production `DispatchHook` during the `syscall` phase. See below. |
| `consoles`         | `&'static [ConsoleDevice]`        | The installed system console list backing the standard streams (the `stream_write` / `stream_read` syscalls, `AGENTS.md` §20): index 0 the primary console, each further entry an independent console with its own session context (`plans/PI.md` P11). Defaults to the empty fail-closed `NO_CONSOLES` until the arch port installs its discovered list via `with_consoles`. |

`BootInfo::validate()` runs at the top of `kernel_main` and reports any
violation as a `BootInfoError`; the kernel then logs a `PhaseFailed`
record under the `log` phase and halts.

## Audit event catalogue

`kernel/core` owns the `4_000..5_000` event-id range:

| ID   | Level | Name                    | Sink   |
|-----:|-------|-------------------------|--------|
| 4000 | Info  | `KERNEL_BOOT_STARTED`   | audit  |
| 4001 | Info  | `KERNEL_PHASE_STARTED`  | log    |
| 4002 | Info  | `KERNEL_PHASE_READY`    | log    |
| 4003 | Error | `KERNEL_PHASE_FAILED`   | audit  |
| 4004 | Info  | `KERNEL_BOOT_COMPLETED` | audit  |
| 4010 | Error | `KERNEL_PANIC`          | audit  |
| 4020 | Error | `SYSCALL_FEATURE_UNAVAILABLE` | audit  |
| 4021 | Error | `SYSCALL_NO_CALLER_CONTEXT`   | audit  |
| 4030 | Info  | `PROCESS_SPAWNED`            | audit  |
| 4031 | Error | `PROCESS_SPAWN_DENIED`       | audit  |
| 4032 | Error | `PROCESS_SPAWN_FAILED`       | audit  |
| 4040 | Info  | `USERS_DB_LOADED`            | audit  |
| 4041 | Error | `USERS_DB_REJECTED`          | audit  |
| 4042 | Info  | `DRIVER_STORE_SCANNED`      | audit  |

The `USERS_DB_*` pair reports the boot-time users-database load
(`rustos_kernel_core::users::load_users_db`, `plans/PI.md` P11): given the
mounted root volume's `FilesystemRead` + `FilesystemSecurity` driver, the
kernel resolves `/System/Security/Users` through the VFS's §5.3-checked
per-inode delegation (`Vfs::read_via_secured`, with the root mount backed
by the volume's driver), bounds the file against the format's 64 KiB
maximum *before* reading it, and parses it through the fail-closed
`rustos-users` parser. The read runs under the kernel's bootstrap identity
— `uid 0`, no capabilities, no bypass (`AGENTS.md` §5.1) — and any refusal
leaves the system with **no** database, so every login refuses
(`AGENTS.md` §5.4.5). The `users_db_qemu_aarch64` vertical proves the path
end to end on the QEMU `virt` board against a real (emulated) virtio-blk
users-root volume.

A boot path that mounts the root volume installs the loaded database into
the production dispatch hook through one kernel-neutral seam (the §17.4
boundary between the architecture-neutral install policy and the
board-specific storage bring-up that *produces* the mounted driver).
`rustos_kernel_core::users::load_users_db_source` shares that exact read,
parse, and `USERS_DB_*` audit path with `load_users_db` (`AGENTS.md` §2.2)
but retains the validated `users-v1` *text* in a `HeldUsersDbSource` — the
canonical bytes the `users_db_read` syscall serves verbatim, never a
re-serialisation. The boot path `Box::leak`s that holder and hands it to
`BootInfo::with_users_db`; `kernel_main` threads it into the
`KernelDispatchHook` so `users_db_read` serves it to a `CAP_USERS_READ`
caller. The held bytes are salted credential records: the holder zeroes
them on drop (`AGENTS.md` §4) and its `Debug` is redacted (length only).
Until a boot path calls `with_users_db`, the handover keeps the
fail-closed `NULL_USERS_DB` default and `users_db_read` returns
`NotImplemented`, so login refuses every attempt rather than inventing
accounts (`AGENTS.md` §5.4.5).

The composition that *produces* the mounted driver `load_users_db_source`
reads from is `rustos_kernel::root_mount::unlock_root_and_load_users`
(`plans/PI.md` P11, Chunk A) — the one layer permitted to name both the
`rustfs` driver and `kernel/core` (`rustos-kernel`, `Layer::Tooling`,
`AGENTS.md` §17.4). Given the plaintext `root.unlock` key-derivation
descriptor read off the FAT boot partition, the passphrase the operator
typed at the console, and the encrypted root block device, it: decodes the
descriptor fail-closed (`UnlockDescriptor::decode`, §5.4.3); derives the
volume key from the passphrase (PBKDF2-HMAC-SHA256), holding it in a
`Zeroizing` wrapper so it is wiped on drop (`AGENTS.md` §4 — the audited
`zeroize` crate, no hand-rolled primitive); mounts the encrypted root
(`RustFs::open`), a wrong passphrase refused with `PermissionDenied` and no
plaintext fallback (§4 / §5.4); then runs `load_users_db_source`. Every
refusal is audited (the bin-crate ids `4133` `ROOT_MOUNT_UNLOCKED` /
`4134` `ROOT_MOUNT_REJECTED`, in the shared `4_000` range; no passphrase,
key, or volume byte is ever logged, §19.4) and yields **no** database, so a
root that cannot be unlocked serves none (§5.4.5).

The first of those three inputs — the plaintext `root.unlock`
key-derivation descriptor — is recovered off the FAT boot partition by
`rustos_kernel::root_mount::read_root_unlock_descriptor`, which mounts the
partition through the **same** real FAT32 driver `tools/mkimage` authored
it with (one on-disk definition for writer and reader, `AGENTS.md` §2.2;
the file name is the shared `rustos_drv_fs_rustfs::ROOT_UNLOCK_NAME`
constant). The descriptor is a fixed-length record, so the read is strictly
bounded and fail-closed (§5.4 / §24.4): the entry's size is checked to be
exactly `UNLOCK_DESCRIPTOR_LEN` *before* a byte is read — rejecting both a
truncated and an over-long file — and the bytes are still re-validated by
`UnlockDescriptor::decode` before they drive any key derivation (§5.4.3).

`rustos_kernel::root_mount::mount_root_and_load_users` is the single
boot-path entry that ties those two halves together: given the brought-up
FAT boot-partition and encrypted-root block devices and the typed
passphrase, it reads the descriptor (`read_root_unlock_descriptor`) and,
on success, threads it straight into `unlock_root_and_load_users`, so the
boot path neither re-threads the descriptor buffer nor reconciles two
error taxonomies itself (`AGENTS.md` §2.2). A descriptor that cannot be
read off the boot partition is audited (`4134` `ROOT_MOUNT_REJECTED`) and
returned as `RootMountError::DescriptorRead` — the encrypted root is never
touched and no database is served (§2.9 / §5.4.5).

`ROOT_STORAGE_AUTOLOAD` (the bin-crate id `4135`, in the shared `4_000`
range) reports the **root-storage bind gate**
(`rustos_kernel::root_storage`, `AGENTS.md` §18.3 / §18.6, `plans/PI.md`
P11 Chunk B-2) — the storage analogue of the keyboard bind gate. As the
aarch64 boot path enters the kernel core it walks the discovered hardware
tree and resolves which node carries the bootstrap root block device
against the in-kernel floor catalogue (`rustos_kernel::driver_catalog`),
through the **same** shared `lib/devmatch` policy the user-space `devmgr`
autoloader uses: the kernel binds a block driver because that driver's
signed bind table matched a discovered node, never because it *hunted* for
a disk (§18.5). The record names the bound driver path, the node id, and
the bind priority; a tree with no block device leaves the root unbound
(informational, §18.4), and a tree with more than one distinct block
device fails closed as ambiguous (`Error`) rather than guessing which
volume is the root (§2.9). The gate is **resolution only** — it mounts
nothing — so it changes no boot behaviour beyond the audit record and the
metal-confirmed boot is unaffected (§2.17).

`rustos_kernel::root_mount::unlock_root_disk_interactively` is the
device-independent **interactive unlock policy** the in-kernel unlock
kthread runs once the board has brought up the root block device and the
console keyboard is live (`plans/PI.md` P11 Chunk B-2). It is generic over
the `Block` disk and takes the console write/read halves as the object-safe
`rustos_kernel_core::{ConsoleWrite, ConsoleRead}` seams, so it names no
architecture or device type (§17.4) and is host-tested with a mock console
over the same MBR + encrypted-`RustFS` disk fixture `tools/mkimage` writes
(§2.2). Each attempt prompts `Root passphrase:`, reads one line into a
zeroized, fixed-length on-stack buffer (`MAX_PASSPHRASE_LEN`; the secret
never reaches the heap, a log, or memory beyond the attempt, §4 / §19.4),
and runs `mount_root_disk_and_load_users`. On success the loaded database
is published into the set-once `LateUsersDb` cell (`4136`
`ROOT_UNLOCK_INSTALLED`) so login can authenticate. A wrong passphrase
(`Mount(PermissionDenied)`) is audited (`4137` `ROOT_UNLOCK_RETRY`, no
oracle) and retried up to `MAX_UNLOCK_ATTEMPTS` (5, the User-chosen policy);
the budget is bounded, never an infinite loop (§2.1). Any other error is
structural (no table, no boot/root partition, an unreadable/invalid
descriptor, a corrupt database) and gives up at once, as does a console
read fault; every give-up path (`4138` `ROOT_UNLOCK_GAVE_UP`, with a
secret-free `cause`) leaves the cell empty, so every login is refused until
the next boot (§2.9 / §5.4.5).

The remaining board-specific bring-up that *supplies* the block device and
drives this policy — bringing up the bound block + filesystem driver
through an in-kernel block DriverHost behind the signed §8 load gate, and
wiring the primary-console prompt + the `&'static LateUsersDb` dispatch
hook into the init seam — is wired into the boot path next (`plans/PI.md`
P11 Chunk B-2): `virtio-blk` proves it on `-M virt`, EMMC2 on metal
(`plans/PI.md` §0.4 / P8).

`DRIVER_STORE_SCANNED` reports the boot-time enumeration of the
`/System/Drivers/` signed-driver store
(`rustos_kernel_core::driver_store::enumerate_driver_store`, `AGENTS.md`
§18.3 / §18.6, `plans/PI.md` P10 Stage 4.HW item 5). Mirroring the
users-database read, it walks the mounted root volume's `FilesystemRead` +
`FilesystemSecurity` driver under the uid-0 bootstrap identity (no §5.1
bypass), collecting the image path of every regular file under
`/System/Drivers/` — the candidate paths the user-space scan
(`rustos_drvhost::store::scan_store`) reads, bind-decodes, and hands to the
`devmgr` autoloader. It only finds paths; it never reads, parses, or trusts
a bundle — the load gate (`rustos_drvhost::Host::load`) verifies a bundle
only when it wins a node (§18.6). The walk is bounded
(`MAX_STORE_DEPTH` / `MAX_STORE_DRIVERS`, §24.4) and fail-closed: a missing
store, an unreadable sub-directory, or a malformed entry simply contributes
fewer paths and never aborts the boot (§18.4 / §2.9). The single record
carries the `drivers` count found and the `skipped` count refused.

The **Sink** column names the `BootInfo`-supplied channel each record
is emitted on. Audit-class boot lifecycle events
(`AGENTS.md` §5.4.4 — security-relevant decisions) route through
`audit_sink`; phase-timeline events route through the diagnostic
`log_sink`. Production kernels typically wire both sinks to the same
COM1 backend; the QEMU integration test bin
(`tests/integration/kernel_arch_boot`) intercepts the audit channel
only, observing `KERNEL_BOOT_COMPLETED` to flip the QEMU
`isa-debug-exit` device.

New events take the next free identifier and require an update to this
table and the event catalogue in `kernel/core/src/audit.rs`.

## Testing

The crate is fully host-testable. `kernel/core/tests/kernel_main.rs`
drives the entry point with a `TestArch + TestSink` and asserts:

* the happy-path init order matches this document exactly,
* a failing `mem` phase logs `PhaseFailed { phase = "mem",
  cause = "mem_out_of_memory" }` and halts,
* a malformed `BootInfo` is reported under the `log` phase with the
  documented `cause` string,
* `handle_panic` emits exactly one `KERNEL_PANIC` record and halts.

The mock `TestArch::halt` panics with a sentinel message so
`std::panic::catch_unwind` can observe the halt without blocking the
test runner; this scaffold is gated behind the `test-arch` Cargo
feature and never links into a production build.

## Stage 2 integration tests

`AGENTS.md` §7 routes cross-crate / end-to-end tests to the top-level
`tests/` tree. Stage 2 enrols two:

- `tests/integration/memory_isolation/src/main.rs` — a freestanding
  x86_64 kernel binary that builds two distinct page tables, switches
  CR3 between them, and asserts that the attacker context faults
  (`#PF`, error code 0, CR2 == target VA) while the victim's frame
  remains intact. Executes under QEMU through
  `cargo xtask test --qemu`. See
  [`platform/x86_64.md`](../platform/x86_64.md).
- `tests/integration/scheduler_stress/tests/stress.rs` — a 20 000-task
  / 4-simulated-core deadlock-free, bounded-latency stress over the
  `rustos-kernel-sched-mlfq` public surface. Runs as part of the host-side
  `cargo xtask test` pass today. Promoting it to QEMU is on the
  Stage 3a sub-checklist in `PLAN.md` (depends on SMP + APIC timer +
  IPI).

`tools/qemu` is the audited gateway between the host build and any
QEMU integration test (multiboot2 ISO via `grub-mkrescue`,
`isa-debug-exit` device, strict per-test timeouts, no retries —
`AGENTS.md` §7).

## Stage 3a (c7) — first production `KernelArch` impl

`kernel/arch/x86_64::kernel_arch::X86_64Arch` (Stage 3a (c7-arch),
PLAN.md) is the first production implementation of the Arch HAL trait
`rustos_arch_api::SchedulerArch` (`AGENTS.md` §17.2) in tree;
`kernel/sched/api` re-exports that trait, so the impl also satisfies
every `rustos_kernel_sched_api::SchedulerArch` bound. The trait impl is
feature-gated behind `rustos-arch-x86_64`'s opt-in `sched-arch`
feature — see
[`platform/x86_64.md`](../platform/x86_64.md#stage-3a-c7-arch--schedulerarch-impl-for-x86_64)
for the bare-metal / host semantics and the dependency rationale.

The matching `rustos_kernel_core::KernelArch::halt` impl now ships in
the Stage 3a (c7-bin) `rustos-kernel` binary at
`kernel/rustos-kernel/` — see
[`platform/x86_64.md`](../platform/x86_64.md#stage-3a-c7-bin--rustos-kernel-binary)
for the boot pipeline. The bin crate's `BinArch(X86_64Arch)` newtype
satisfies Rust's orphan rules; `BinArch::halt` forwards to the free
function `kernel_arch::halt()` and is compile-time-pinned to the
`-> !` signature by `_BIN_ARCH_HALT_RETURNS_NEVER`. The bin's
`kernel_main(multiboot_info)` body composes everything: Multiboot2 →
ACPI/MADT → `BootMemoryMap`; `X86_64Arch::new`; per-CPU init; the
fail-closed syscall-dispatch callback installed *before* `syscall` is
enabled on any CPU; finally `BootInfo::new` + forward to
`rustos_kernel_core::kernel_main`.

The companion QEMU integration test
`tests/integration/kernel_arch_boot` boots the binary end-to-end
under QEMU on `-smp 1` and exits successfully on observing
`AuditEvent::BootCompleted` (`EventId(4004)`).

### Stage 2.7 follow-up

The (c7-bin) dispatch callback is **fail-closed**: it parks the CPU
forever if any `syscall` ever reaches it (none does at this stage —
there is no user space yet). When Stage 2.7 lands a
`SyscallHandlers` impl and per-CPU `CallerContext` plumbing, the
callback body is replaced with a forwarder to
`rustos_kernel_syscall::Dispatcher::dispatch`. The ABI is pinned at
compile time by `_DISPATCH_SIGNATURE_PINNED`, so the swap is a
body-only change with no public-surface impact.
