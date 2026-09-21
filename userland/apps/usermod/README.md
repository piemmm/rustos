# `tairix-usermod` — modify a user account

A `plans/APPS.md` command app registered at
`/System/Commands/usermod.app/Run`, so the shell resolves the bare word
`usermod` to it. It changes an account's identity fields (`-c`, `-d`,
`-s`, `-g`, `-G`), its lock state (`-L`/`-U`), and its capability grant
ceiling (`--grants`).

## Two shapes worth knowing

**An identity edit is a whole-record replacement.** The `users_admin`
`ModifyUser` operation carries the account's complete non-security field
set, because a half-specified record is not one the kernel engine could
verify. So `usermod` reads the account's current record through the
shared listing first and resends every field nobody named unchanged; an
account the listing does not hold is refused before anything is sent.

**Each switch is its own operation.** The kernel applies one at a time,
whole-or-nothing, and there is no combined "modify and lock" request —
nor should there be, since the two are validated by different rules. A
line asking for several therefore issues several in one fixed order
(identity, then grants, then lock state), stops at the first refusal, and
says which step it was *and* that an earlier change may already be in
effect. That is the same posture GNU `usermod` has; stating it is the
difference between a diagnosis and a mystery.

`--grants` is a TAIRiX concept with no coreutils counterpart, so it is
spelled long-only and cannot collide with a shadow-utils switch. The
kernel refuses any capability the calling account does not itself hold.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary, the shared `tairix-help` engine, the
shared `tairix-useradmin` client, and the `tairix-users` account policy,
so it never links a kernel or driver crate. Its manifest requests
`CAP_USER_ADMIN`, which sits above the session baseline: for an account
without it the intersection strips the capability and every operation is
refused at dispatch. `CAP_FS_ACCESS` exists solely so the short-help
switches can read the bundle's own `Help/` tree.
