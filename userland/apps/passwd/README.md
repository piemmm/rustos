# `tairix-passwd` — set an account's password

A `plans/APPS.md` command app registered at
`/System/Commands/passwd.app/Run`, so the shell resolves the bare word
`passwd` to it. It replaces the named account's stored password record.

## No plaintext crosses the syscall

The tool prompts twice with terminal echo off, hashes what was typed into
a salted PBKDF2 record through the shared `lib/users` builder — the salt
drawn from the kernel CSPRNG through the unprivileged `sys:random`
resource — and sends the record. Both plaintext buffers are zeroised the
moment the record exists, and the shared client scrubs its own encode
buffer, because a salted record is credential material even though it is
not a password.

## Two deliberate differences from GNU `passwd`

**The account name is required.** GNU's operand-less form changes the
caller's own password, which on TAIRiX would need an unprivileged
self-service path that does not exist: the whole `users_admin` interface
is gated on `CAP_USER_ADMIN`, and carving out "your own record" is a
security-model change rather than a convenience.

**`--record` takes a ready record.** A graphical caller cannot type into
this tool — its standard input is closed when it runs under the
supervisor's elevated seam — so it hashes the password itself and hands
the finished record over. The record is validated as a well-formed one
before it is stored, so a malformed word is refused rather than becoming
a credential nothing can ever match.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary, the shared `tairix-help` engine, the
shared `tairix-useradmin` client, and the `tairix-users` account policy,
so it never links a kernel or driver crate. Its manifest requests
`CAP_USER_ADMIN`, which sits above the session baseline: for an account
without it the intersection strips the capability and every operation is
refused at dispatch. `CAP_FS_ACCESS` exists solely so the short-help
switches can read the bundle's own `Help/` tree.
