# `rustos-files` — filesystem browser

Stage 7 deliverable (`AGENTS.md` §10, `PLAN.md` Stage 7,
`plans/APPWIN.md` AW3). The default graphical file manager: the `Run`
entry-point binary of the on-disk `files.app` bundle the desktop
session's start menu spawns. Installed as a `.app` bundle in the system
app store (`AGENTS.md` §16.2/§16.5).

## What this crate is

Only the program. Everything with behaviour worth testing lives in
shared, host-tested crates the binary composes over the live syscalls:

- the directory-browser **engine** — the transactional navigation model
  (`Browser`), the themed listing renderer (`render`), the validated
  path spelling, and the `VfsDirectorySource` composition — is the
  shared `lib/browse` crate (`rustos-browse`), the same engine the
  desktop session's trusted file picker drives (`plans/APPWIN.md` AW5),
  so the file manager and the picker can never diverge;
- the window channel's client half (`WindowClient` / `WindowEvents`) is
  `lib/window`;
- the runtime (`_start`, allocator, syscall wrappers, the shared
  `read_dir_all` listing call) is `lib/rt`.

## What the program wires

One `shm_create`d frame region granted to the reserved window endpoint
(the zero-copy window surface), one `port_bind`-bound event mailbox the
app **parks** on through its wait-set (every accepted event
authenticated against the kernel-attested session identity the create
reply named), and the `WindowClient` calls over `ipc_call`. Keyboard
navigation drives the browser (`Down`/`Up` select, `Enter` opens a
directory, `Backspace` goes up); a `CloseRequested` from the desktop
ends the program cleanly; every bring-up refusal exits fail-loud with a
reserved code and a stated reason on `stderr` (`AGENTS.md` §2.24).

## Capabilities

`CAP_CONSOLE_WRITE` (fail-loud diagnostics), `CAP_FS_ACCESS` (its
directory listings — every read still permission-checked per inode
under the launching user's identity), and `CAP_SHM` (the granted window
frame region). See `AppInfo.toml`.

## Test surface

The engine's behaviour is exhaustively host-tested in `lib/browse`
(`cargo test -p rustos-browse`); this package carries only the inert
host stub the workspace tooling compiles.
