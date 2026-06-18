# Userland driver host (`rustos-drvhost`)

`rustos-drvhost` is the userland service that owns the lifecycle of every
`.rxe` driver module on a running RustOS system. It is the single point at
which an image is parsed, verified, capability-checked, and handed an
environment to register itself against (`AGENTS.md` §8). The host runs in
user space by default (`AGENTS.md` §4); the same code path also services
`kind = "in-kernel"` drivers by demanding `CAP_DRV_KERNEL` in addition to
the universal `CAP_DRV_LOAD`.

## Public surface

```rust
use rustos_drvhost::{Host, HostConfig, HostError, ImageSource, DriverSpawner};
use rustos_virtio::VirtioHostFactory; // the host's virtio seam lives in lib/virtio

fn drive_one_module(deps: &ServiceDeps) -> Result<(), HostError> {
    let cfg = HostConfig {
        trusted_signers: &[/* Ed25519PublicKey ... */],
        syscall_table_hash: [/* SHA-256 of the kernel's syscall table */],
        accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
        source: &deps.source,   // impl ImageSource
        spawner: &deps.spawner, // impl DriverSpawner
        sink: &deps.audit_sink, // impl rustos_log::Sink
        virtio_host_factory: None, // or Some(&dyn VirtioHostFactory)
        mmio_mapper: None,         // or Some(&dyn rustos_abi::MmioMapper)
    };
    let mut host = Host::new(cfg);

    let handle = host.load("/d/my-driver", &deps.caller_caps)?;
    // ... driver is now live ...
    let new_handle = host.reload(handle, &deps.caller_caps)?;
    host.unload(new_handle)?;
    Ok(())
}
```

The seven types above are the entire public surface; everything else
(envelope splitter, signature primitive, audit emitter) is internal and
covered by unit tests in the crate itself.

### Trust anchor

`HostConfig::trusted_signers` is the *closed* list of Ed25519 public keys
the host accepts. A manifest signed by any key not on this list is
refused with `HostError::UntrustedSigner` *before* signature verification
is even attempted. There is no notion of "trust on first use" — adding
or removing a trust anchor requires restarting the host process.

### Syscall table fingerprint

`HostConfig::syscall_table_hash` is the SHA-256 of the kernel's encoded
syscall table (the same hash `lib/abi`'s `ENCODED_TABLE` produces and
`kernel/syscall::table` independently stores). A manifest carrying any
other value is refused with `HostError::SyscallHashMismatch`; this is
how `abi-vN` binaries are detected on an `abi-vM` host (`AGENTS.md` §9).

### Image source

`ImageSource::read(path, buf) -> Result<(), Errno>` is the abstraction
over `.rxe` storage. Production deployments wire it to the filesystem
driver; tests wire it to an in-memory map; the Stage 4 QEMU integration
test wires it to a `.rodata` blob baked in by `build.rs`. `path` is an
opaque `&str` chosen by the caller; the host stores it verbatim so that
`reload(handle)` can re-fetch the same image without re-deriving its
location.

### Driver spawner

`DriverSpawner::spawn_and_register(ctx) -> Result<DriverHandle,
SpawnRegisterError>` is the seam at which a verified manifest's
registration is completed in its own protection domain. The
`SpawnContext` carries the verified manifest, the image payload, the
granted-capability `DriverHost` view, and the granted capability set as
a value (`SpawnContext.granted` — what `ctx.host` answers
`has_capability` from, surfaced so a process-spawning spawner can create
the driver with exactly that authority and no more, `AGENTS.md` §4). The
production implementation (`PLAN.md` Stage 4.HW) spawns the payload into
a fresh process (`kernel/mem::build_process_image` → spawn) and completes
the `register()` handshake over IPC; tests and QEMU verticals register a
known entry point in-process through `ctx.host`. The seam returns the
*outcome* of registration rather than an entry point, so the host
never holds a pointer into the driver image.

The `kernel/rustos-kernel/src/driver_spawn_loader.rs` `SpawnDriverLoader`
is the production process-spawning loader: it implements the device
manager's `DriverLoader` seam, so the autoload walk drives it directly,
runs this same `Host::load` gate on the discovered `kind = UserSpace`
image, and spawns the verified payload through the architecture
`DriverProcessSpawn` seam — minting the new process one device-resource
grant per `HwResource` its matched hardware-tree node requested
(`KernelSpawnCtx.grants`, `AGENTS.md` §18.3) and nothing more. The
`tests/integration/driver_spawn_qemu_aarch64` vertical proves that full
devmgr → signed-gate → spawn → grant path on the `virt` board (a virtio
node stands in for the metal controller).

The IPC half of that handshake is defined: the spawned driver reads
the reply endpoint id from its startup arguments (`rustos_rt::arg`),
encodes a
[`DriverRegisterReply`](../abi/driver_traits.md#driverregisterreply)
(`registered(handle)` / `failed(error)`), and sends it with the
`rustos-rt` `ipc_send` wrapper; the host decodes it fail-closed and
treats the reported handle as informational only (it mints its own).

The kernel-side spawn path behind that handshake is in place on
aarch64: the parameterised driver spawn is the `kernel/core`
`ProcessSpawn::spawn_with(rxe, ctx, caps, args)` trait method — the
driver-spawn analogue of `spawn(EmbeddedProgram, ctx)`, taking the
verified image bytes, the manifest∩caller capability set, and the
matched node's grants riding on `ctx` (§18.3). Exposing it on the trait
lets a scheduler-agnostic caller (a generic `kernel_main` holding `&dyn
ProcessSpawn`) spawn a driver without naming the port's spawn mechanism
or the selected scheduler (`AGENTS.md` §17.1 / §17.4); the default fails
closed with `Errno::NotImplemented` (§2.9). The aarch64 producer
(`kernel/rustos-kernel/src/aarch64/spawn_producer.rs`) implements it —
the `spawn` syscall path delegates to it with the fixed session grant —
and `kernel/core` exports `KernelSpawnCtx`, the same admit context the
`spawn` syscall
handler uses (scheduler admit, capability-record insert, address-space
+ standard-stream + resource-limit registration, parent/child wait
link) — so a kernel-side (host-driven) driver spawn drives the
identical production path. The
`tests/integration/driver_spawn_qemu_aarch64` vertical proves the full
chain on the `virt` board: a verified `/System/Drivers/` payload is
spawned with driver-class capabilities and the reply endpoint id in
`arg(1)`; the stub completes the register reply over the production,
capability-gated `ipc_send` path while the host side polls
`Port::recv` under a bounded cooperative budget.

The spawner is *only* invoked after every other verification gate has
cleared (`AGENTS.md` §5.4 — fail closed): a misbehaving spawner
cannot widen the host's authority. Those gates are, in order: image
parse, syscall-table hash, signature (over header, capability body,
bind table, *and* the payload — so a `kind = UserSpace` driver's program
is authenticated, never substitutable after signing, `AGENTS.md` §8 /
§2.17), capability subset/kind checks, and a fail-closed
decode of every
[`DriverBindKey`](../abi/driver_traits.md#driverbindkey) bind-table
entry — a malformed table never reaches the device manager
(`AGENTS.md` §18.3).

### Virtio host factory

`HostConfig::virtio_host_factory: Option<&dyn VirtioHostFactory>` is
the seam at which the host supplies a per-driver
`rustos_abi::driver::VirtioHost` for the duration of a single
`register()` call. The `VirtioHostFactory` trait itself lives in the
bus-agnostic `lib/virtio` host seam, so both the userland host and any
kernel-side implementation depend on `lib/*` rather than on each other
(`AGENTS.md` §17.4); its `mint` is handed the driver's granted
capabilities as a `&dyn rustos_abi::CapabilityQuery` so the seam need
not name `lib/caps`. The driver retrieves the host through the new
`DriverHost::virtio_host(&self) -> Option<&dyn VirtioHost>` accessor
(an `abi-v1` internal addition; the public `register(host: &dyn
DriverHost) -> Result<DriverHandle, DriverError>` entry point per
`AGENTS.md` §8 is unchanged) and stashes it inside its own driver
struct.

A factory that returns `None` is indistinguishable from leaving
`virtio_host_factory` unset; both shapes cause `host.virtio_host()`
to report `None`, and a virtio-class driver's `register()` should
then refuse to load (it has no transport). The concrete production
factory is `KernelVirtioFactory`
(`kernel/virtio/src/virtio_factory.rs`, Stage 4.D Item
2-tail.4): it mints a `KernelVirtioHost` (`kernel/virtio`) backed by
a freshly-carved per-driver `DmaPool` and the calling task's
`TaskCapabilities`. The concrete factory lives in `kernel/virtio`,
not in `drvhost`, so the host crate stays free of every `kernel/*`
dependency; and because the `VirtioHostFactory` trait it implements
lives in `lib/virtio`, the kernel crate in turn never depends on the
userland host (`AGENTS.md` §3 / §17.4). The mock factory
used in unit tests mints a `MockHost` whose allocations leak for the
duration of the test process.

The factory is consulted **after** every other verification gate
has cleared and **before** `register()` is called, so a driver
load that is going to be refused never reaches the factory and a
factory that refuses (returns `None`) never widens the host's
authority. The boxed virtio host lives on the
`verify_and_bind` stack frame and is dropped immediately after
`register()` returns; the caller's per-driver `DmaPool` slots are
reclaimed at that drop. The host that calls `register()` is the
sole owner of the box — drivers must not retain `&dyn VirtioHost`
references across the `register()` boundary (the lifetime in the
trait signature prevents this at compile time).

### MMIO mapper

`HostConfig::mmio_mapper: Option<&dyn rustos_abi::MmioMapper>` is the
seam at which the host supplies a bus driver the means to map a
device's register window. A driver retrieves it through the
`DriverHost::mmio_mapper(&self) -> Option<&dyn MmioMapper>` accessor
(an `abi-v1` internal addition alongside `virtio_host`; the public
`register` entry point is unchanged) and maps each window through the
capability-gated `MmioMapper::map_window` — never a pointer the driver
synthesises (`AGENTS.md` §4). A host that leaves the slot unset reports
`None`, and a bus driver's `register()` must then refuse to load
(`AGENTS.md` §5.4).

The concrete production mapper is `KernelMmioMapper` (`kernel/virtio`),
which routes every request through the capability-gated
`rustos_kernel_sec::map_mmio` path; it lives in `kernel/virtio`, not in
`drvhost`, so the host crate stays free of every `kernel/*` dependency
and the `MmioMapper` trait it implements lives in `lib/abi` (`AGENTS.md`
§17.4). Unlike the per-load boxed virtio host, the mapper is borrowed
for the host's lifetime and lent unchanged to every driver load — its
own window bitmap is the per-load state. The in-kernel composition that
wires both seams at once (a bus driver that maps register windows *and*
carves a DMA region — the VL805 xHCI behind the BCM2711 PCIe root
complex, `plans/PI.md` P10) is `rustos_kernel::run_with_driver_host`
(`kernel/rustos-kernel/src/driver_host.rs`).

### In-kernel chain admission (the Pi 4 USB keyboard)

The Pi 4 USB-keyboard chain (`pcie_brcm` → `bus_usb` → `usb_hid`) is the
first *production* caller of the `Host::load` gate (`plans/PI.md` P10
5c-ii). Its drivers are statically linked, and their §8 `register()`
entries are admission-only (a `CAP_DRV_LOAD` check returning a marker),
so `kernel/rustos-kernel/src/driver_loader.rs`'s `ChainDriverLoader`
admits each one through a plain `Host` (no MMIO/DMA host: the real
register-window mapping and DMA carve run afterwards over the keyboard
service's own capability-gated host). The signed manifest images and the
trust anchor are produced at build time by `build.rs`
(`emit_signed_driver_manifests`): each `DriverManifest` is `kind =
InKernel`, stamped with the kernel's `SYSCALL_TABLE_HASH`, requests
`CAP_DRV_LOAD`, carries the driver crate's own `BIND_KEYS`, and is
Ed25519-signed with the build's deterministic driver-signing key
(`KERNEL_DRIVER_SIGNING_SEED`); the matching public key is embedded as
the kernel's sole driver trust anchor. The seed has a single home in
`kernel/rustos-kernel/src/build_support.rs` (the dependency-free
`#[path]` module the build script pulls in), so a fixture or image build
that lays a *kernel-trusted* bundle into the driver store signs from the
same definition rather than a copy (`AGENTS.md` §2.2). The kernel trusts
only the drivers its own reproducible build signed; secrecy of the seed
buys nothing
(`AGENTS.md` §19.3), so it is committed and the signatures stay
bit-reproducible. The keyboard service admits the two bus drivers before
bring-up and re-matches the enumerated HID child against the driver
catalogue to admit `usb_hid` before feeding input — fail closed at each
step (`AGENTS.md` §5.4).

### Signed-store scan

RustOS ships no compiled-in list of *which* drivers exist: the
discovered driver set is found at runtime by scanning the installed
signed bundles under `/System/Drivers/` (`AGENTS.md` §18.6). The
`rustos_drvhost::store` module is that scan. Given the bundle paths a
caller enumerated (a VFS directory walk of `/System/Drivers/` in
production; the bin-crate boot wiring is the one layer that may name
both `drvhost` and `devmgr`, `AGENTS.md` §17.4) and an `ImageSource`,
`scan_store(source, paths, sink) -> DriverStore` reads each bundle,
parses its `.rxe` manifest with the same `ParsedImage` splitter the
load gate uses (so the match data can never drift from the gate's view
of the bytes, `AGENTS.md` §2.2), and decodes its bind table fail-closed.
Each accepted bundle becomes an owned `ScannedDriver`, and
`DriverStore::candidates()` lends the borrowed `DriverCandidate` slice
that `rustos_devmgr::DeviceManager::autoload` matches against the
hardware tree.

The scan is a **match** step only and grants no authority. Building a
candidate from a bundle's bind table is *necessary but never
sufficient* to run it: the Ed25519 signature, syscall-hash, capability
set, and `kind` are still verified by the load gate (`Host::load`)
when — and only when — that candidate wins a hardware-tree node
(`AGENTS.md` §18.6). A bundle that is unreadable, has a malformed
manifest, or whose bind table fails to decode is **skipped and logged**,
never fatal: one bad bundle cannot block the rest of the boot
(`AGENTS.md` §18.4 / §5.4).

#### Reading the bundle bytes off the root volume

In production the bundle bytes live on the mounted root volume under
`/System/Drivers/`. The kernel finds *which* paths exist with
`rustos_kernel_core::enumerate_driver_store` (a §5.3-checked VFS walk
under the uid-0 bootstrap identity), and reads the bytes of a chosen
bundle with `rustos_kernel_core::DriverImageReader`: it builds the
root-backed VFS **once** (`AGENTS.md` §2.16), then per call validates
that the path lies strictly within `/System/Drivers/`, bounds the file
against `MAX_DRIVER_IMAGE_LEN` (a 16 MiB §24.4 validation cap) *before*
reading a byte, reads the whole file, and **appends** it to the caller's
buffer — failing closed and leaving the buffer untouched on any refusal
(`AGENTS.md` §5.4 / §2.9). Every read runs under the uid-0, no-capability
bootstrap identity: a bundle is reachable only because its stored §5.3
record makes it readable to that identity, never through an ambient
bypass (`AGENTS.md` §5.1).

The `ImageSource` trait lives in `drvhost` (userland), and the §17.4
layering forbids `kernel/core` from depending on it, so the bin crate —
the one layer that may name `drvhost` — supplies the thin adapter
`rustos_kernel::driver_store_source::VfsImageSource`. It holds the
`DriverImageReader` plus the root-volume filesystem driver (behind a
`RefCell`, because `ImageSource::read` is `&self` while the driver needs
`&mut`; the scan is single-threaded and pulls one bundle at a time, so
the borrow never overlaps) and simply delegates each `read` to the
kernel-core reader. It adds no authority of its own.

#### Autoloading by discovery

`rustos_kernel::driver_autoload::autoload_drivers` is the one boot-wiring
composition that turns the discovered hardware tree and the installed
signed store into running user-space drivers — the "drivers in user space
by discovery" steady state (`AGENTS.md` §4 / §18). It adds no policy of its
own; it threads the building blocks above together:

1. `drvhost::store::scan_store` reads each `/System/Drivers/` bundle path
   (from `enumerate_driver_store`) through the `VfsImageSource` and decodes
   its manifest bind table fail-closed — a **match** step only (§18.6).
2. `devmgr::DeviceManager::autoload` resolves every tree node against those
   candidates through the shared `lib/devmatch` policy (§18.3), leaving an
   unmatched node unbound and logged (§18.4).
3. Each winning node's driver is loaded through
   `rustos_kernel::driver_spawn_loader::SpawnDriverLoader`, which runs the
   signed `Host::load` gate and **spawns** the verified payload into its own
   process, minting it one device-resource grant per `HwResource` the
   matched node requested — and nothing more (§18.3 / §4).

A candidate that fails the signed gate fails *that node* closed and the
walk continues, so one bad bundle never blocks the boot (§5.4 / §23.1). The
function lives in the kernel binary — the one layer that may name both
`devmgr` and `drvhost` (§17.4) — and is the staged production entry the boot
path drives once the root volume that backs the store is mounted in
production (`plans/PI.md` P10 5d-2-ii "Remaining").

`rustos_kernel::driver_autoload::autoload_from_mounted_root` is the thin
production glue that drives `autoload_drivers` straight off a **mounted root
volume**. Given the root volume's filesystem driver `fs` (rustfs on a real
install), it sources both halves of the scan from `fs` itself: it walks the
store with `enumerate_driver_store(fs, …)` for the bundle paths, then builds
a `VfsImageSource` over the *same* `fs` for the bundle bytes. The two reads
of `fs` are strictly sequential — the path walk returns owned `String`s and
releases its `&mut` borrow before the source takes it — so the single
mutable borrow never overlaps. It adds no policy and fails closed (returning
`VfsError`) only if the private root mount cannot be built; a store that is
missing, empty, or full of malformed bundles simply binds nothing and
returns `Ok` (§18.4 / §2.9). This is the composition the boot path's
root-unlock path calls once the encrypted root is mounted.

### Audit sink

Every state transition emits one structured `rustos_log::Event` with a
stable `EventId` from `rustos_drvhost::events`:

| `EventId` | Meaning                                              |
|----------:|------------------------------------------------------|
| `7001`    | driver loaded                                        |
| `7002`    | load rejected — manifest decode failed               |
| `7003`    | load rejected — syscall table hash mismatch          |
| `7004`    | load rejected — signer key not on trust anchor list  |
| `7005`    | load rejected — Ed25519 signature verification failed |
| `7006`    | load rejected — requested capabilities exceed caller |
| `7007`    | load rejected — `InKernel` without `CAP_DRV_KERNEL`  |
| `7008`    | load rejected — caller lacks `CAP_DRV_LOAD`          |
| `7009`    | load rejected — spawner has no driver for manifest   |
| `7010`    | load rejected — driver `register()` returned an error |
| `7011`    | load rejected — bind-table entry failed to decode    |
| `7020`    | driver unloaded                                      |
| `7021`    | driver reloaded                                      |
| `7030`    | signed-store bundle accepted as autoload candidate   |
| `7031`    | signed-store bundle skipped during scan              |

The identifiers are part of the `7000..8000` range reserved for the
driver host (`AGENTS.md` §2.5). They are pinned by an in-tree
uniqueness test and may never be re-numbered.

## Error mapping

`HostError::as_errno(self) -> rustos_abi::Errno` is total: every
variant has a stable counterpart in `abi-v1`. Callers wrapping the host
behind a syscall surface the result without inventing new error codes.

## Stability tier

`experimental` (`AGENTS.md` §6). The wire formats consumed (manifest
header, capability body, and bind table) are pinned by `lib/abi`'s
`DriverManifest` / `DriverBindKey`; the host's own public Rust API
freezes once Stage 4 lands its first real driver.

## Security model

The host never decides on its own that a caller is authorised. Every
`load` takes the caller's `CapabilitySet` explicitly, intersects the
driver's request against it, and refuses any superset (`AGENTS.md`
§5.2 — capabilities can be delegated but never widened). The
host-owned `DriverHandle` is the unforgeable proof that a load
succeeded; the value the driver's `register()` returned is informational
only and never replaces the host's freshly minted handle.

Buffers that held the manifest signature or capability bitmap are
wiped through a volatile clear primitive (`zeroize::secure_clear`)
before their backing allocation is freed (`AGENTS.md` §4).
