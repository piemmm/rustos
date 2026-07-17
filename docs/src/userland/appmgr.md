# Application-manager service

`userland/system/appmgr` (`tairix-appmgr`) is the user-space service that
loads and launches an installed `/Apps/<Name>.app/` bundle on behalf of a
user (`AGENTS.md` §16.4, §16.5). It is installed to
`/System/Services/appmgr`. Loading runs in user space because the matching
and capability policy is not kernel code (microkernel-leaning, §4); the
kernel only enforces the resulting capability ceiling at exec time.

## What it is

`appmgr` is the **user-space consumer** of the shared bundle load gate. The
security-relevant judgement — validating the fixed bundle layout, verifying
the signed `AppInfo` manifest and content hash, computing the granted
capability ceiling as the launching user's grants intersected with the
manifest request, and enforcing the dynamic-loader shared-library policy —
lives in the shared [`tairix-appload`](../lib/appload.md) crate, so the one
gate is used by both this service and the kernel boot-floor spawn path, never
re-implemented (§2.2, §17.4).

This crate re-exports that gate (`AppLoader`, `AppLoaderConfig`,
`BundleStore`, `Verifier`, `LoadedApp`, `AppError`). The service binary wires
the real VFS-backed `BundleStore` and `lib/crypto`-backed `Verifier` and
drives `AppLoader::load` for user-initiated launches, then spawns the verified
`Run` binary with the computed ceiling.

For the full load pipeline, dynamic-loader policy, seams, and audit events,
see the [`tairix-appload` page](../lib/appload.md).

The crate is `no_std` and depends only on `tairix-appload` (itself `lib/*`),
so the userland service never links a kernel or driver crate (§17.4); it has
no `unsafe` and no `unwrap`/`expect`/`panic!` in production paths.
