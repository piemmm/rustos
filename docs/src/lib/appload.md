# `rustos-appload` — application-bundle load gate

`lib/appload` (`rustos-appload`) is the one place a `<Name>.app/` bundle is
*judged* before it may be launched (`AGENTS.md` §16.4, §16.5). It is a shared
`lib/*` crate so the same gate is used by both the kernel boot-floor spawn
path and the user-space [application-manager service](../userland/appmgr.md),
never re-implemented (§2.2, §17.4). The gate runs in this shared layer because
the layout, signature, and capability policy is not architecture- or
kernel-specific; the kernel only enforces the resulting capability ceiling at
exec time.

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
4. **Signature** — verifies the Ed25519 signature over the whole manifest
   except the signature field (the header prefix concatenated with the
   capability/MIME body, so a swapped capability id breaks it) through the
   `Verifier` seam. A failure is `AppError::Signature`.
5. **Contents** — compares the bundle's actual content hash (from the
   `BundleStore` seam) against the hash the signature covers, in constant
   time. A mismatch is `AppError::ContentHashMismatch` (§16.5).
6. **Authority** — decodes the requested capabilities and grants the
   **intersection** of that request with `user_grants`; ambient authority
   is forbidden (§4, §5.2), so the loader never widens a request.
7. **Run image** — reads the `Run` binary and validates it through
   `rustos_abi::rxe::LoadImage::parse` with the kernel's syscall hash as the
   expected CFI tag, enforcing the §19.2 hardening invariants (PIE, W^X,
   CFI tag); a malformed image or CFI-tag mismatch is `AppError::RunImage`.

On success it returns a `LoadedApp` carrying the bundle identity, the
validated `Run` entry-point path, the granted capability ceiling, and the
resolved needed libraries. The loader never executes anything: spawning the
verified binary with that ceiling is the caller's job (the same load gate
`init`/`drvhost` use).

`AppLoader::resolve_library(bundle, reference)` applies the §16.4
dynamic-loader policy: a reference resolves only against the bundle's own
`Libraries/` or `/System/Libraries/`; anything else is `AppError::Library`.

## Seams

The two operations that touch the outside world are injected, so the
security-relevant code is testable without a kernel:

- `BundleStore` — lists a bundle's top-level entries, reads its `AppInfo`
  bytes, reads its `Run` image, and hashes its contents. Backed by the VFS
  on a running system.
- `Verifier` — verifies an Ed25519 signature. Backed by `lib/crypto`
  (§2.12 — the one place cryptographic primitives live).

## Audit

Every decision is recorded through `lib/log` (§19.4) in the reserved
`EventId` range `11000..12000`: `APP_LOADED`, `APP_LAYOUT_REJECTED`,
`APP_MANIFEST_INVALID`, `APP_INTERFACE_MISMATCH`, `APP_SIGNATURE_INVALID`,
`APP_CONTENT_MISMATCH`, `APP_STORE_ERROR`, `LIBRARY_RESOLVED`,
`LIBRARY_REFUSED`, and `APP_RUN_IMAGE_INVALID`.

The crate is `no_std` (with `alloc`) and depends only on `rustos-abi`,
`rustos-caps`, and `rustos-log` (§17.4); it has no `unsafe` and no
`unwrap`/`expect`/`panic!` in production paths. Stability tier:
`experimental`.
