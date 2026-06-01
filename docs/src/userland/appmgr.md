# Application bundle loader

`userland/system/appmgr` (`rustos-appmgr`) is the user-space service that
decides whether a `/Apps/<Name>.app/` bundle may be launched and with what
authority (`AGENTS.md` §16.4, §16.5). It is installed to
`/System/Services/appmgr`. Loading runs in user space because the matching
and capability policy is not kernel code (microkernel-leaning, §4); the
kernel only enforces the resulting capability ceiling at exec time.

## What it does

`AppLoader::load(bundle, user_grants)` runs a fail-closed pipeline
(`AGENTS.md` §5.4) and stops at the first problem:

1. **Layout** — reads the bundle's top-level entries and validates them
   against the fixed [`AppInfo` ABI](../abi/appinfo.md) set (`BundleEntry`).
   A stray, duplicate, or missing-mandatory entry is `AppError::Layout`.
2. **Manifest** — decodes the signed `AppInfoHeader` and checks its ABI
   version. A bad manifest is `AppError::Manifest`.
3. **Interface** — compares the manifest's declared syscall-table hash
   against the kernel's, in constant time. A mismatch is
   `AppError::InterfaceHashMismatch` (§9 / §19.2).
4. **Signature** — verifies the Ed25519 signature over the manifest's
   signed range through the `Verifier` seam. A failure is
   `AppError::Signature`.
5. **Contents** — compares the bundle's actual content hash (from the
   `BundleStore` seam) against the hash the signature covers, in constant
   time. A mismatch is `AppError::ContentHashMismatch` (§16.5).
6. **Authority** — decodes the requested capabilities and grants the
   **intersection** of that request with `user_grants`; ambient authority
   is forbidden (§4, §5.2), so the loader never widens a request.

On success it returns a `LoadedApp` carrying the bundle identity, the
validated `Run` entry-point path, and the granted capability ceiling. The
loader never executes anything: spawning the verified binary with that
ceiling is the caller's job (the same load gate `init`/`drvhost` use).

`AppLoader::resolve_library(bundle, reference)` applies the §16.4
dynamic-loader policy: a reference resolves only against the bundle's own
`Libraries/` or `/System/Libraries/`; anything else is `AppError::Library`.

## Seams

The two operations that touch the outside world are injected, so the
security-relevant code is testable without a kernel:

- `BundleStore` — lists a bundle's top-level entries, reads its `AppInfo`
  bytes, and hashes its contents. Backed by the VFS on a running system.
- `Verifier` — verifies an Ed25519 signature. Backed by `lib/crypto`
  (§2.12 — the one place cryptographic primitives live).

## Audit

Every decision is recorded through `lib/log` (§19.4) in the reserved
`EventId` range `11000..12000`: `APP_LOADED`, `APP_LAYOUT_REJECTED`,
`APP_MANIFEST_INVALID`, `APP_INTERFACE_MISMATCH`, `APP_SIGNATURE_INVALID`,
`APP_CONTENT_MISMATCH`, `APP_STORE_ERROR`, `LIBRARY_RESOLVED`, and
`LIBRARY_REFUSED`.

The crate is `no_std` (with `alloc`) and depends only on `rustos-abi`,
`rustos-caps`, and `rustos-log` (§17.4); it has no `unsafe` and no
`unwrap`/`expect`/`panic!` in production paths.
