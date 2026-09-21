# `tairix-userdel` — delete a user account

A `plans/APPS.md` command app registered at
`/System/Commands/userdel.app/Run`, so the shell resolves the bare word
`userdel` to it. It removes one account from the user database that
persists under `/System/Security/Users`.

`userdel` is a parser and a presenter, not a policy point: what may be
removed is the database's decision, taken kernel-side under the caller's
attested identity. It refuses the deletion of the last active account
that may administer users, so a system can never be left with no way to
administer it.

`-h`/`-?` render the tool's own short help from its bundled `Help/` tree
through the shared `lib/help` engine, in the locale the inherited `LANG`
variable names, falling back to the usage banner when the tree is
unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary, the shared `tairix-help` engine, the
shared `tairix-useradmin` client, and the `tairix-users` account policy,
so it never links a kernel or driver crate. Its manifest requests
`CAP_USER_ADMIN`, which sits above the session baseline: for an account
without it the intersection strips the capability and every operation is
refused at dispatch. `CAP_FS_ACCESS` exists solely so the short-help
switches can read the bundle's own `Help/` tree.
