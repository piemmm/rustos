# tairix-sysconfig

Stability tier: **experimental**.

The boot-time system-configuration store engine: the one definition of the
`/System/Settings/Configuration/system.conf` document — its line grammar,
the closed key registry (`os.loginType` and the `cache.*` caching
switches), each key's typed value set, the bounded fail-closed parser, and
the canonical render.

The `configure` command app (`userland/apps/configure`) writes the store
through this engine; boot-time consumers read it through the same engine
after the encrypted root volume is unlocked, so producer and consumer can
never diverge — the login service for `os.loginType`, and the kernel's
cache-admission control (`kernel/core::syscfg` → `CacheControl`) for the
`cache.*` switches (a master `cache.all` ceiling over the per-class
`cache.filesystem` / `cache.block` / `cache.transform` / `cache.semantic`).
The crate performs no
I/O and holds no authority: file access goes through the secured VFS under
the caller's own kernel-attested identity, and the per-inode policy on
`/System/Settings` decides who may write.

`no_std` + `alloc` (the render); host-unit-tested in `src/lib.rs`.
