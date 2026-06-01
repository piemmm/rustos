# `rustos-appmgr` — application bundle loader

Stage 6 deliverable (`AGENTS.md` §16.4, §16.5). The user-space service that
decides whether a `/Apps/<Name>.app/` bundle may be launched and with what
authority. Installed to `/System/Services/appmgr`.

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
   fixed `rustos_abi::BundleEntry` set, with the mandatory `AppInfo` and
   `Run` present (`AppError::Layout`).
2. **Manifest** — decode the signed `AppInfoHeader` and check its ABI version
   (`AppError::Manifest`).
3. **Interface** — constant-time compare of the manifest's syscall-table hash
   against the kernel's (`AppError::InterfaceHashMismatch`, §9 / §19.2).
4. **Signature** — verify the Ed25519 signature over the manifest's signed
   range via the `Verifier` seam (`AppError::Signature`).
5. **Contents** — constant-time compare of the bundle's content hash (from
   the `BundleStore` seam) against the hash the signature covers
   (`AppError::ContentHashMismatch`, §16.5).
6. **Authority** — grant the **intersection** of the manifest's requested
   capabilities with the launching user's grants; ambient authority is
   forbidden (§4, §5.2), so a request is never widened.

On success it returns a `LoadedApp` (bundle id / name / version, the `Run`
entry-point path, and the granted `CapabilitySet`).

## Dynamic-loader policy (`AppLoader::resolve_library`)

A shared-library reference resolves only against the bundle's own
`Libraries/` directory or `/System/Libraries/`; a reference with a `..`
component or one pointing anywhere else is refused (`AppError::Library`,
`AGENTS.md` §16.4).

## Seams

Injected, so the security-relevant code is testable without a kernel:

- `BundleStore` — `entries` / `read_appinfo` / `content_hash`. Backed by the
  VFS on a running system.
- `Verifier::verify(signed, signature, signer_pubkey)` — Ed25519
  verification. Backed by `lib/crypto` (§2.12).

## Audit events

Reserved `EventId` range `11000..12000`:

- `11001 APP_LOADED` — a bundle was accepted (Info).
- `11002 APP_LAYOUT_REJECTED` — layout outside the fixed set (Warn).
- `11003 APP_MANIFEST_INVALID` — manifest undecodable / bad ABI (Warn).
- `11004 APP_INTERFACE_MISMATCH` — syscall-hash mismatch (Warn).
- `11005 APP_SIGNATURE_INVALID` — signature did not verify (Warn).
- `11006 APP_CONTENT_MISMATCH` — contents differ from the signed hash (Warn).
- `11007 APP_STORE_ERROR` — the bundle could not be read (Warn).
- `11008 LIBRARY_RESOLVED` — a library reference resolved within policy (Info).
- `11009 LIBRARY_REFUSED` — a library reference violated the §16.4 policy (Warn).

## Layering & safety

`no_std` (with `alloc`), depends only on `rustos-abi`, `rustos-caps`, and
`rustos-log` (all `lib/*`), so a userland service never links a kernel or
driver crate (`AGENTS.md` §17.4). No `unsafe`, no `unwrap`/`expect`/`panic!`
in production paths (`AGENTS.md` §2.9).

## Test surface

`cargo test -p rustos-appmgr` (16 unit tests): the happy path with
capability intersection; the minimal (`AppInfo` + `Run`) layout; fail-closed
unknown-entry / missing-`Run` layouts; an undecodable manifest; an
unsupported ABI version; a syscall-hash mismatch; a bad signature; a
content-hash mismatch; a store error; a truncated capability body; the
in-policy and out-of-policy library resolutions; plus the `EventId`
range/uniqueness invariants.
