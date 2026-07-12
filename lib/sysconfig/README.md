# rustos-sysconfig

Stability tier: **experimental**.

The boot-time system-configuration store engine: the one definition of the
`/System/Settings/Configuration/system.conf` document — its line grammar,
the closed key registry (today `os.loginType`, `text`/`graphical`), each
key's typed value set, the bounded fail-closed parser, and the canonical
render.

The `configure` command app (`userland/apps/configure`) writes the store
through this engine; boot-time consumers (the login service's session-type
default) read it through the same engine after the encrypted root volume is
unlocked, so producer and consumer can never diverge. The crate performs no
I/O and holds no authority: file access goes through the secured VFS under
the caller's own kernel-attested identity, and the per-inode policy on
`/System/Settings` decides who may write.

`no_std` + `alloc` (the render); host-unit-tested in `src/lib.rs`.
