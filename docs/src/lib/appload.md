# `tairix-appload` — application-bundle load gate

`lib/appload` (`tairix-appload`) is the one place a `<Name>.app/` bundle is
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
5. **Publisher** — resolves the bundle's stable developer identity
   (`PublisherId`). A self-published manifest needs no further signature: its
   publisher key *is* the signing key the trust root just admitted. A
   *delegated* manifest must carry the publisher's Ed25519 signature over
   `AppInfoHeader::publisher_cert_message`, which names both the bundle
   identifier and the signing key, so a certificate cannot be lifted onto
   another bundle or another key; a malformed binding is
   `AppError::Manifest(SignatureInvalid)` and a certificate that does not
   verify is `AppError::PublisherCert`. The publisher key is deliberately not
   trust-rooted — it is a claim the certificate makes unforgeable, not an
   authority — and what gives it meaning is the per-app store pinning the
   identity it first saw (`plans/APPDATA.md`). This is why a release may be
   re-signed with a fresh build key without the app losing the settings,
   secrets, and blobs it stored.
6. **Contents** — compares the bundle's actual content hash (from the
   `BundleStore` seam) against the hash the signature covers, in constant
   time. A mismatch is `AppError::ContentHashMismatch` (§16.5).
7. **Authority** — decodes the requested capabilities and grants the
   **intersection** of that request with `user_grants`; ambient authority
   is forbidden (§4, §5.2), so the loader never widens a request.
8. **Run image** — reads the `Run` binary and validates it through
   `tairix_abi::rxe::LoadImage::parse` with the kernel's syscall hash as the
   expected CFI tag, enforcing the §19.2 hardening invariants (PIE, W^X,
   CFI tag); a malformed image or CFI-tag mismatch is `AppError::RunImage`.

On success it returns a `LoadedApp` carrying the bundle identity (identifier,
name, version, and verified `PublisherId`), the validated `Run` entry-point
path, the granted capability ceiling, and the resolved needed libraries. The
loader never executes anything: spawning the verified binary with that ceiling
is the caller's job (the same load gate `init`/`drvhost` use).

The kernel's spawn path retains the accepted `LoadedApp` for bundles on
the immutable read-only system stores in the semantic launch cache
(`tairix_kernel_core::launch_cache::LaunchCache`, `plans/SMARTRAM.md`
SMART4): the gate runs once per boot per store bundle, and every later
launch serves the cached result after re-authorising the caller's own
read of the entry point — the cache stores no caller-dependent decision
and is drained under memory pressure
(`docs/src/architecture/memory.md` §7j).

`AppLoader::resolve_library(bundle, reference)` applies the §16.4
dynamic-loader policy: a reference resolves only against the bundle's own
`Libraries/` or `/System/Libraries/`; anything else is `AppError::Library`.

## Seams

The two operations that touch the outside world are injected, so the
security-relevant code is testable without a kernel:

- `BundleStore` — lists a bundle's top-level entries, reads its `AppInfo`
  bytes, reads its `Run` image, and hashes its contents. Backed by the VFS
  on a running system.
- `Verifier` — decides whether a manifest signer is trusted to sign a bundle
  here, and verifies its signature. The seam exists for the *trust root*,
  which genuinely differs per host: the kernel's store path pins the build's
  embedded app-signing anchor. It is not the loader's general cryptography
  provider — the publisher delegation certificate carries no host policy, so
  the loader verifies it directly through `lib/crypto` rather than letting
  each host re-implement it.
- `Clock` — reports monotonic nanoseconds, read only to time the load and
  verify phases for the `APP_LOADED` record. Backed by the architecture
  monotonic clock in the kernel; audit-only, so no load decision depends on
  it.

## Audit

Every decision is recorded through `lib/log` (§19.4) in the reserved
`EventId` range `11000..12000`: `APP_LOADED`, `APP_LAYOUT_REJECTED`,
`APP_MANIFEST_INVALID`, `APP_INTERFACE_MISMATCH`, `APP_SIGNATURE_INVALID`,
`APP_PUBLISHER_INVALID`, `APP_CONTENT_MISMATCH`, `APP_STORE_ERROR`,
`LIBRARY_RESOLVED`, `LIBRARY_REFUSED`, and `APP_RUN_IMAGE_INVALID`.

The `APP_LOADED` record additionally carries a `load` duration (time spent
reading the bundle off the store — the "getting it from disk" cost) and a
`verify` duration (the remaining layout / manifest / interface-hash /
signature / publisher / content-hash / run-image checking), so a slow first
launch is diagnosable from the audit log.

The crate is `no_std` (with `alloc`) and depends only on `tairix-abi`,
`tairix-caps`, `tairix-crypto`, and `tairix-log` (§17.4); it has no `unsafe`
and no `unwrap`/`expect`/`panic!` in production paths. Stability tier:
`experimental`.
