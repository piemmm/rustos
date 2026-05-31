# Filesystem permissions

The VFS enforces the `AGENTS.md` §5.3 model: **POSIX mode bits, plus
ACLs, plus a per-inode capability gate**. Every operation routes its
decision through one function (`Metadata::authorize`), which fails closed
and **never branches on `uid == 0`** — authority comes from capability
grants, ACL entries, and mode bits, never from a magic user id
(`AGENTS.md` §5.1).

## Inode metadata

Each inode stores:

- `owner` (uid) and `group` (gid),
- POSIX `mode` bits (owner/group/other `rwx`, plus `setuid`/`setgid`/
  sticky),
- an optional `required_cap` capability gate,
- an `acl`: a list of explicit allow/deny entries, each naming a user or
  group, an access right, and a decision.

## The three layers, in order

A request for `Read`, `Write`, or `Execute` access is decided as follows:

1. **Capability gate.** If the inode declares a `required_cap` and the
   caller does not hold it, access is denied *regardless of the mode
   bits*. A file marked `CAP_AUDIT_READ` is unreadable at mode `0644` by a
   caller without that capability.
2. **ACL.** Among entries matching the caller (by uid, or by membership in
   a named group) for the requested access: an explicit **deny** wins; an
   explicit **allow** grants it. With no matching entry, the decision falls
   through to the mode bits.
3. **Mode bits.** The owner / owning-group / other triad is selected by the
   caller's identity, and the requested `rwx` bit is checked.

Only the inode's **owner** may change its capability gate
(`Vfs::set_required_cap`).

## Traversal

Resolving a path requires **search (execute) permission on every directory
descended through**, mirroring POSIX. `stat`-style `metadata` therefore
needs search permission along the path but no permission on the target
itself; `read` additionally needs read permission on the file; `mkdir`,
`create_file`, and `remove` need write permission on the *parent*
directory.

## Errors

Permission failures return `VfsError::PermissionDenied`. A write blocked by
a read-only mount returns `VfsError::ReadOnly`; a reserved top-level name
returns `VfsError::ReservedPath`. `VfsError::to_errno` maps these to the
stable user/kernel `Errno` at the syscall boundary (read-only and
reserved-name refusals both surface as `PermissionDenied`).
