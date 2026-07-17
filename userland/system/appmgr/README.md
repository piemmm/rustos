# `tairix-appmgr` — application-manager service

Stage 6 deliverable (`AGENTS.md` §16.4, §16.5). The user-space service that
loads and launches an installed `/Apps/<Name>.app/` bundle on behalf of a
user. Installed to `/System/Services/appmgr`.

## What this crate is

`appmgr` is the **user-space consumer** of the shared bundle load gate. The
security-relevant judgement — validating the fixed bundle layout, verifying
the signed `AppInfo` manifest and content hash, computing the granted
capability set as the launching user's grants intersected with the manifest
request, and enforcing the dynamic-loader shared-library policy — lives in the
shared `lib/appload` crate (`tairix-appload`), so the one gate is used by both
this service and the kernel boot-floor spawn path, never re-implemented
(`AGENTS.md` §2.2, §17.4).

This crate re-exports that gate (`AppLoader`, `AppLoaderConfig`, `BundleStore`,
`Verifier`, `LoadedApp`, `AppError`, and the `bundle`/`error`/`events`/
`loader` modules) so a consumer of `appmgr` sees the same surface. The service
binary that wires the real VFS-backed `BundleStore` and `lib/crypto`-backed
`Verifier` and drives `AppLoader::load` for user-initiated launches is built
on top of it.

For the load pipeline, dynamic-loader policy, seams, audit events, and test
surface, see `lib/appload` (`tairix-appload`).

## Layering & safety

`no_std`, depends only on `tairix-appload` (itself `lib/*`), so the userland
service never links a kernel or driver crate (`AGENTS.md` §17.4). No `unsafe`,
no `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).
