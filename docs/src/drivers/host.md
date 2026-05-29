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
use rustos_drvhost::{Host, HostConfig, HostError, ImageSource, EntryResolver};

let cfg = HostConfig {
    trusted_signers: &[/* Ed25519PublicKey ... */],
    syscall_table_hash: [/* SHA-256 of the kernel's syscall table */],
    accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
    source: &my_source,     // impl ImageSource
    resolver: &my_resolver, // impl EntryResolver
    sink: &my_audit_sink,   // impl rustos_log::Sink
};
let mut host = Host::new(cfg);

let handle = host.load("/d/my-driver", &caller_caps)?;
// ... driver is now live ...
let new_handle = host.reload(handle, &caller_caps)?;
host.unload(new_handle)?;
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

### Entry resolver

`EntryResolver::resolve(manifest, payload) -> Option<DriverEntry>` is
the seam at which a verified manifest becomes an executable
`register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError>`
function pointer. In production this resolver loads the image into a
fresh process and `dlsym`s the `register` symbol; in tests it returns
a Rust function pointer keyed on whatever subset of the manifest the
test cares about.

The resolver is *only* invoked after every other verification gate has
cleared (`AGENTS.md` §5.4 — fail closed): a misbehaving resolver
cannot widen the host's authority.

### Audit sink

Every state transition emits one structured `rustos_log::Event` with a
stable `EventId` from `rustos_drvhost::events`:

| `EventId` | Meaning                                              |
|----------:|------------------------------------------------------|
| `7001`    | driver loaded                                        |
| `7002`    | load rejected — manifest decode failed               |
| `7003`    | load rejected — syscall table hash mismatch          |
| `7004`    | load rejected — signer key not on trust anchor list  |
| `7005`    | load rejected — Ed25519 signature verification failed|
| `7006`    | load rejected — requested capabilities exceed caller |
| `7007`    | load rejected — `InKernel` without `CAP_DRV_KERNEL`  |
| `7008`    | load rejected — caller lacks `CAP_DRV_LOAD`          |
| `7009`    | load rejected — resolver could not bind manifest     |
| `7010`    | load rejected — driver `register()` returned an error|
| `7020`    | driver unloaded                                      |
| `7021`    | driver reloaded                                      |

The identifiers are part of the `7000..8000` range reserved for the
driver host (`AGENTS.md` §2.5). They are pinned by an in-tree
uniqueness test and may never be re-numbered.

## Error mapping

`HostError::as_errno(self) -> rustos_abi::Errno` is total: every
variant has a stable counterpart in `abi-v1`. Callers wrapping the host
behind a syscall surface the result without inventing new error codes.

## Stability tier

`experimental` (`AGENTS.md` §6). The wire formats consumed (manifest
header and capability body) are already frozen by `lib/abi`'s
`DriverManifest`; the host's own public Rust API freezes once Stage 4
lands its first real driver.

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
