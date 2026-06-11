# Driver lifecycle

A driver progresses through a fixed sequence of observable states
between the moment a caller hands an image path to
`rustos_drvhost::Host::load` and the moment the host returns a
`DriverHandle`. Each state is a verification gate;
a failure at any step yields a typed `HostError`, an audit record on
the configured `rustos_log::Sink`, and *no* mutation of the host's
loaded-driver table (`AGENTS.md` §5.4 — fail closed).

```text
   Image bytes (.rxe)
        │
        ▼
┌──────────────────┐   ImageTruncated / ManifestInvalid
│ 1. envelope parse│ ──────────────────────────────────► reject (7002)
└──────────────────┘
        │
        ▼
┌──────────────────┐   SyscallHashMismatch
│ 2. abi pin check │ ──────────────────────────────────► reject (7003)
└──────────────────┘
        │
        ▼
┌──────────────────┐   UntrustedSigner / SignatureInvalid
│ 3./4. signature  │ ──────────────────────────────────► reject (7004/7005)
└──────────────────┘
        │
        ▼
┌──────────────────┐   KernelKindForbidden /
│ 5. capability    │   CapabilityEscalation /
│    gate          │   CapabilityOutOfRange
└──────────────────┘ ──────────────────────────────────► reject (7006/7007)
        │
        ▼
┌──────────────────┐   BindKeyInvalid(_)
│ 6. bind table    │ ──────────────────────────────────► reject (7011)
│    decode        │
└──────────────────┘
        │
        ▼
┌──────────────────┐   UnknownDriver
│ 7. spawner       │ ──────────────────────────────────► reject (7009)
│    hand-off      │
└──────────────────┘
        │
        ▼
┌──────────────────┐   DriverRegisterFailed(_)
│ 8. register()    │ ──────────────────────────────────► reject (7010)
└──────────────────┘
        │
        ▼
   handle issued
   record installed
   audit (7001)
```

## Operations

### `Host::load(path, caller_caps)`

1. Refuses immediately with `LoadCapabilityMissing` (audit `7008`) if
   `caller_caps` does not contain `CAP_DRV_LOAD`.
2. Reads the image via `ImageSource::read`. A source-supplied
   `Errno::NotFound` surfaces as `HostError::SourceFailed(Errno::NotFound)`.
3. Runs the eight verification gates in order.
4. Issues a fresh `DriverHandle`, calls the driver's `register()`,
   installs a `LoadedRecord`, and emits the `DRIVER_LOADED` audit
   record.

### `Host::unload(handle)`

1. Locates the record by handle. `HandleNotFound` if absent.
2. Removes the record from the host's table. Its `Drop` impl runs
   `zeroize::secure_clear` on the stored image bytes — the manifest
   signature, capability body, and bind table are guaranteed to be
   wiped before the underlying allocation is freed (`AGENTS.md` §4).
3. Emits the `DRIVER_UNLOADED` audit record.

### `Host::reload(handle, caller_caps)`

1. Locates the record by handle. `HandleNotFound` if absent.
2. Drives a *fresh* `load` against the recorded path. If anything
   fails the host returns the underlying error and leaves the old
   record installed (transient errors must not deprive the system of a
   working driver).
3. On success, removes the old record (its sensitive buffer is wiped),
   emits the `DRIVER_RELOADED` audit record, and returns the new
   handle. **The old handle is invalidated** — callers must update any
   state keyed on it.

## Capability checks

| Manifest `kind` | Caller requirement                                              |
|-----------------|-----------------------------------------------------------------|
| `UserSpace`     | `CAP_DRV_LOAD` ⊇ caller, requested caps ⊆ caller                |
| `InKernel`      | `CAP_DRV_LOAD` *and* `CAP_DRV_KERNEL` ⊆ caller, requested caps ⊆ caller |

`requested caps ⊆ caller` is the only delegation rule in the system
(`AGENTS.md` §5.2). It is enforced by `rustos_caps::CapabilitySet`'s
`is_subset_of`; the same primitive that backs `CapabilityToken::verify`.

## Audit timeline

Every `Host::load`, `Host::unload`, and `Host::reload` emits at least
one structured event on the sink configured in `HostConfig::sink`.
A successful `reload` emits two events on the sink (one
`DRIVER_LOADED` from the embedded `load`, then one `DRIVER_RELOADED`)
so consumers can distinguish a reload from an unrelated load that
happened to use the same path. A failed `reload` emits only the
appropriate reject record from the embedded `load` attempt; the old
record stays in place and produces no event of its own.

## Sensitive buffer hygiene

The host wipes every buffer that held the manifest signature or
capability bitmap before its backing allocation is freed. There are
three such buffers per `load`:

1. The staging `Vec<u8>` returned by `ImageSource::read` — wiped at
   the end of `Host::load` once the bytes have been copied into the
   `LoadedRecord`.
2. The transient `Vec<u8>` that holds `header[..signed_end] ||
   capability_body || bind_table` for the Ed25519 verifier — wiped
   between verification and the next pipeline step.
3. The `LoadedRecord::image` buffer that backs the live record —
   wiped on `Drop` (i.e. on `Host::unload` or successful
   `Host::reload`).

The wipe primitive is `rustos_drvhost::zeroize::secure_clear`, which
combines `core::ptr::write_volatile` with a sequentially-consistent
`compiler_fence` so neither the optimiser nor the surrounding scope's
`Drop` can re-order the writes away. The primitive is the only
`unsafe` block in the crate and is covered by an in-crate unit test.
