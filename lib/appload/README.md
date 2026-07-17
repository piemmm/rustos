# `tairix-appload` — application-bundle load gate

Shared `lib/*` crate (`AGENTS.md` §16.4, §16.5, §17.4). The one place a
`<Name>.app/` bundle is judged before it may be launched — the gate the
kernel boot-floor spawn path and the user-space `appmgr` service (`userland/
system/appmgr`) both link, rather than each re-implementing it (§2.2).

## What this crate is

The **bundle gate** — it validates a bundle's layout, verifies its signed
`AppInfo` manifest and contents, computes the capability ceiling the app may
run with, and enforces the dynamic-loader shared-library policy. It does not
*execute* anything: spawning the verified `Run` binary with the computed
ceiling is the caller's job (the same load gate `init`/`drvhost` use,
`AGENTS.md` §8, §9).

## Load pipeline (`AppLoader::load`)

Fails closed at the first problem (`AGENTS.md` §5.4):

1. **Layout** — the bundle's top-level entries must be exactly drawn from the
   fixed `tairix_abi::BundleEntry` set, with the mandatory `AppInfo` and
   `Run` present (`AppError::Layout`).
2. **Manifest** — decode the signed `AppInfoHeader` and check its ABI version
   (`AppError::Manifest`).
3. **Interface** — constant-time compare of the manifest's syscall-table hash
   against the kernel's (`AppError::InterfaceHashMismatch`, §9 / §19.2).
4. **Signature** — verify the Ed25519 signature over the manifest's signed
   range via the `Verifier` seam (`AppError::Signature`).
5. **Contents** — the `BundleStore::contents` seam hashes every file the
   signature covers **and**, in that same walk, returns the entry-point
   `Run` image (the `Run` binary is one of the hashed files, so it is read
   from disk exactly once — never a second time for the entry image). The
   content hash is compared constant-time against the hash the signature
   covers (`AppError::ContentHashMismatch`, §16.5); the `Run` bytes are
   thereby authenticated by that same signed hash.
6. **Authority** — grant the **intersection** of the manifest's requested
   capabilities with the launching user's grants; ambient authority is
   forbidden (§4, §5.2), so a request is never widened.
7. **Run image** — validate the already-read `Run` bytes through
   `tairix_abi::rxe::LoadImage::parse` with the kernel's syscall hash as the
   expected CFI tag (no re-read). This enforces the §19.2 hardening
   invariants (PIE, W^X, CFI tag) on the entry-point binary; a malformed
   image or a CFI-tag mismatch is refused (`AppError::RunImage`).
8. **Needed libraries** — resolve every shared library the `Run` image
   declares it needs (`LoadImage::needed_libraries`) under the §16.4 policy
   below. This binds the curated *System runtime / C ABI* library a non-Rust
   program links; an out-of-tree reference fails closed (`AppError::Library`).

The pipeline is language-agnostic: a C-compiled bundle is validated
identically to a Rust one (`plans/CCOMPAT.md` stage CC4).

On success it returns a `LoadedApp` (bundle id / name / version, the `Run`
entry-point path, the granted `CapabilitySet`, and the resolved needed
libraries with their policy scope).

## Dynamic-loader policy (`AppLoader::resolve_library`)

A shared-library reference resolves only against the bundle's own
`Libraries/` directory or `/System/Libraries/`; a reference with a `..`
component or one pointing anywhere else is refused (`AppError::Library`,
`AGENTS.md` §16.4).

## Seams

Injected, so the security-relevant code is testable without a kernel:

- `BundleStore` — `entries` / `read_appinfo` / `contents` (the single pass
  that both hashes the bundle contents and returns the `Run` image, so the
  entry binary is read from disk once). Backed by the VFS on a running
  system.
- `Verifier::verify(signed, signature, signer_pubkey)` — Ed25519
  verification. Backed by `lib/crypto` (§2.12).
- `Clock::now_ns()` — monotonic nanoseconds, read only to time the load and
  verify phases for the `APP_LOADED` record. Backed by the architecture
  monotonic clock in the kernel; audit-only, so no load decision depends on
  it.

## Audit events

Reserved `EventId` range `11000..12000`:

- `11001 APP_LOADED` — a bundle was accepted (Info). Carries a `load`
  duration (time spent reading the bundle off the store — the "getting it
  from disk" cost) and a `verify` duration (the remaining layout / manifest /
  interface-hash / signature / content-hash / run-image checking), so a slow
  first launch is diagnosable from the audit log.
- `11002 APP_LAYOUT_REJECTED` — layout outside the fixed set (Warn).
- `11003 APP_MANIFEST_INVALID` — manifest undecodable / bad ABI (Warn).
- `11004 APP_INTERFACE_MISMATCH` — syscall-hash mismatch (Warn).
- `11005 APP_SIGNATURE_INVALID` — signature did not verify (Warn).
- `11006 APP_CONTENT_MISMATCH` — contents differ from the signed hash (Warn).
- `11007 APP_STORE_ERROR` — the bundle could not be read (Warn).
- `11008 LIBRARY_RESOLVED` — a library reference resolved within policy (Info).
- `11009 LIBRARY_REFUSED` — a library reference violated the §16.4 policy (Warn).
- `11010 APP_RUN_IMAGE_INVALID` — the `Run` binary is not a valid `rxe` image
  or its CFI tag does not match the kernel's hash (Warn, §9 / §19.2).

## Layering & safety

`no_std` (with `alloc`), depends only on `tairix-abi`, `tairix-caps`, and
`tairix-log` (all `lib/*`), so it links no kernel or driver crate and both a
kernel and a userland consumer may share it (`AGENTS.md` §17.4). No `unsafe`,
no `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).

## Test surface

`cargo test -p tairix-appload`: the happy path with capability intersection;
the minimal (`AppInfo` + `Run`) layout; fail-closed unknown-entry /
missing-`Run` layouts; an undecodable manifest; an unsupported ABI version; a
syscall-hash mismatch; a bad signature; a content-hash mismatch; a store
error; a truncated capability body; the in-policy and out-of-policy library
resolutions; the C-bundle run-image needed-library resolution (runtime from
`/System/Libraries/` + a private bundle library), capability intersection for
a C bundle, an out-of-tree needed library, a run-image CFI mismatch, and a
malformed run image; that the bundle contents (carrying the `Run` image) are
read exactly once per load; the `APP_LOADED` record carrying non-zero `load`
and `verify` durations; plus the `EventId` range/uniqueness invariants.

The kernel-side single-read guarantee is additionally pinned by
`tairix-kernel-core`'s `the_run_binary_is_read_from_disk_exactly_once`, which
counts the `FilesystemService::read` calls the `FsBundleStore` makes for the
`Run` path over a full load.

## Stability

Tier: `experimental`.
