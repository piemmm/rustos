# `tairix-groupdel` — delete a group

A `plans/APPS.md` command app registered at
`/System/Commands/groupdel.app/Run`, so the shell resolves the bare word
`groupdel` to it. It removes one group from the group registry that
persists under `/System/Security/Groups`.

The natural sibling of `groupadd`: the same parser and seam discipline,
narrowed to one operand. What may be removed is the registry's decision —
it refuses a group an account still references, so no account is ever left
naming a group that does not exist.

`-h`/`-?` render the tool's own short help from its bundled `Help/` tree
through the shared `lib/help` engine.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary, the shared `tairix-help` engine, the
shared `tairix-useradmin` client, and the `tairix-users` account policy,
so it never links a kernel or driver crate. Its manifest requests
`CAP_USER_ADMIN`, which sits above the session baseline: for an account
without it the intersection strips the capability and every operation is
refused at dispatch. `CAP_FS_ACCESS` exists solely so the short-help
switches can read the bundle's own `Help/` tree.
