# rustos-test-posix-fs-suite

The `pjdfstest`-equivalent POSIX filesystem conformance suite — the final
`PLAN.md` Stage 5 test deliverable.

It drives the **real** `arxfs` driver (`rustos_drv_fs_arxfs::ARXFS`)
through the **real** `kernel/core::fs::Vfs` policy layer and asserts the
return values and error codes of every filesystem operation the system
exposes. It re-implements no filesystem semantics of its own
(`AGENTS.md` §2.2): the harness in `src/lib.rs` formats a `arxfs` volume
in memory, mounts it at `/Storage/vol` in a default-layout VFS, and the
integration tests under `tests/` exercise it through the
per-inode-security delegation methods.

## Layout

| File                 | Coverage                                                   |
| -------------------- | ---------------------------------------------------------- |
| `src/lib.rs`         | Harness: in-memory `Block`, VFS+`arxfs` builders, helpers |
| `tests/mkdir.rs`     | `mkdir`: create, nested, `EEXIST`, `ENOENT`, `ENOTDIR`     |
| `tests/open_create.rs` | `open`/`read`/`write`: round-trip, sparse, `EISDIR`, EOF  |
| `tests/unlink.rs`    | `unlink`: remove, `ENOENT`, name reuse                     |
| `tests/rmdir.rs`     | `rmdir`: empty removal, `ENOTEMPTY`                        |
| `tests/truncate.rs`  | `truncate`: shrink, grow with zero-fill, `EISDIR`          |
| `tests/readdir_stat.rs` | `readdir`/`stat`: listing, `ENOTDIR`, size/kind        |
| `tests/permission.rs` | §5.3 mode bits, ACL grant, capability gate, search perm   |
| `tests/layout.rs`    | §16 top-level names, read-only `/System`, read-only mount  |
| `tests/errno.rs`     | The stable `Errno` each `VfsError` surfaces                |
| `tests/pathname.rs`  | Absolute-only paths; `.`/`..`, NUL, over-long refused      |

## Scope

This is the filesystem *semantics* layer. The end-to-end verticals
(`tests/integration/{arxfs,fat32}_virtio_blk_pci_x86_64`) prove the same
drivers mount and round-trip over a real virtio-blk device under QEMU;
this crate asserts POSIX-visible behaviour on the host against the
identical driver and VFS code.

The harness is filesystem-agnostic by construction: it talks to the VFS
and a `drivers/filesystem/*` driver behind the frozen ABI traits, so a
second driver can be exercised by swapping the backing constructor.
`arxfs` is the first subject because it stores the per-inode §5.3 record
the permission cases require.

## Running

```
cargo test -p rustos-test-posix-fs-suite
```
