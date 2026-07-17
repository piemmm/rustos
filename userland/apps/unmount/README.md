# `tairix-unmount` — detach a runtime-attached volume

Stability tier: **experimental**.

The `unmount` command app (`plans/DEVICES.md` D4b): `unmount NAME`
resolves the volume mounted under `NAME` (its catalog name or its
mount-point path) through the ungated `sysinfo-v1` `MOUNT_LIST` query —
whose records carry each mount's stable volume identity and
availability — and asks the kernel's `volume_detach` path to take it
out of service. A plain detach flushes first and fails closed; a
surprise-removed (*unavailable-dirty*/*unavailable-lost*) volume
refuses it, keeping its retained uncommitted writes for verified
re-insert. `unmount --force NAME` is the audited force-unmount: the
kernel discards the retained set, retracts the volume, and logs the
deliberate data loss with its own event id (4179). On a healthy volume
`--force` still commits cleanly — nothing is discarded when a clean
flush is possible.

The crate is both the pure `no_std` engine library (parser, resolver,
seams — host-tested against in-memory fixtures) and the freestanding
`Run` program (`src/run.rs`), which wires the shared
`tairix_procinfo::IpcTransport`, the `volume_detach` syscall wrapper,
and the `tairix_help::BundleHelp` short-help source over the inherited
standard streams. The kernel is the policy point: `CAP_FS_MOUNT` is
checked and every decision audited kernel-side; the tool holds no
ambient authority.

`cargo test -p tairix-unmount` drives the parser, the engine (resolve,
force, refusals, the fd-3 `--force` suggestion), and the
help-document switch pinning across every required locale.
