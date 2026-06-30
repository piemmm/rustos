# POSIX conformance suite (`pjdfstest`-equivalent)

The crate `rustos-test-posix-fs-suite`
(`tests/integration/posix_fs_suite`) is RustOS's analogue of
[`pjdfstest`](https://github.com/pjd/pjdfstest): a body of black-box
assertions about the return values and error codes of every filesystem
operation the system exposes. It is the final `PLAN.md` Stage 5 test
deliverable.

## What it exercises

The suite drives the **real** `rustfs` driver
(`rustos_drv_fs_rustfs::RustFs`) through the **real**
`kernel/core::fs::Vfs` policy layer — the identical code paths the
kernel runs — and never re-implements any filesystem semantics
(`AGENTS.md` §2.2). A `rustfs` volume is formatted in memory, mounted at
`/Storage/vol` in a default-layout VFS, and every case asserts behaviour
through the per-inode-security delegation methods (`*_via_secured`).

The test files mirror `pjdfstest`'s operation groups:

| File              | Operation        | Key cases                                             |
| ----------------- | ---------------- | ----------------------------------------------------- |
| `mkdir.rs`        | `mkdir`          | create, nested, `EEXIST`, `ENOENT`, `ENOTDIR`         |
| `open_create.rs`  | `open`/`read`/`write` | create, round-trip across a block boundary, sparse fill, `EISDIR`, `EEXIST`, `ENOENT`, EOF |
| `unlink.rs`       | `unlink`         | remove, `ENOENT`, name reuse, siblings intact         |
| `rmdir.rs`        | `rmdir`          | empty removal, `ENOTEMPTY`, removal once emptied       |
| `truncate.rs`     | `truncate`       | shrink, grow with zero-fill, `EISDIR`, `ENOENT`        |
| `readdir_stat.rs` | `readdir`/`stat` | listing, `ENOTDIR`, reported kind and size            |
| `permission.rs`   | §5.3 model       | owner vs. stranger, the capability gate, an ACL grant, directory search permission, write into a read-only directory |
| `layout.rs`       | §16 layout       | the four top-level directories, a user may create a legacy POSIX name (no refusal), read-only `/System` with writable `Logs`/`Settings`, read-only-mount refusal |
| `errno.rs`        | errno mapping    | the stable `Errno` each `VfsError` surfaces           |
| `pathname.rs`     | namespace        | absolute-only paths; `.`/`..`, NUL, and over-long components refused |

## The permission and capability-gate cases

`permission.rs` is the heart of the suite. Because `rustfs` stores a full
per-inode §5.3 record, the suite can stamp a node's owner, mode bits,
ACL, and optional capability gate (through `RustFs::set_security`) and
then assert the VFS decision. In particular it pins the case the charter
(`AGENTS.md` §5.3) and `PLAN.md` Stage 5 call out by name: a file marked
with a required capability is unreadable without that capability, even at
mode `0644`, and becomes readable once the caller holds it. The decision
never branches on `uid == 0` (§5.1).

## Scope

This suite is the filesystem *semantics* layer. The companion
end-to-end verticals
(`tests/integration/{rustfs,fat32}_virtio_blk_pci_x86_64`) already prove
the same drivers mount and round-trip over a real (emulated) virtio-blk
device under QEMU; the conformance assertions here run on the host
against the very same driver and VFS code, so the two together cover the
filesystem stack from on-disk bytes to POSIX-visible behaviour.

It is filesystem-agnostic by construction: the harness talks to the VFS
and a `drivers/filesystem/*` driver behind the frozen ABI traits, so a
second driver can be exercised by swapping the backing constructor.
`rustfs` is the first subject because it is the native filesystem that
stores the per-inode §5.3 record the permission cases require.
