# `tairix-files` — filesystem browser

Stage 7 deliverable (`AGENTS.md` §10, `PLAN.md` Stage 7,
`plans/APPWIN.md` AW3). The default graphical file manager: the `Run`
entry-point binary of the on-disk `files.app` bundle the taskbar's
permanent Files button spawns. Installed as a `.app` bundle in the system
app store (`AGENTS.md` §16.2/§16.5).

## What this crate is

Only the program. Everything with behaviour worth testing lives in
shared, host-tested crates the binary composes over the live syscalls:

- the directory-browser **engine** — the transactional navigation model
  (`Browser`), the themed listing renderer (`render`), the validated
  path spelling, and the `VfsDirectorySource` composition — is the
  shared `lib/browse` crate (`tairix-browse`), the same engine the
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
navigation drives the browser (`Down`/`Up` select, `Enter` activates the
selection — descends into a directory or launches a selected `<Name>.app`
bundle by spawning the bundle's own `Run` through the ordinary signed
app-load gate (asynchronously, with the launched child reaped on the
wait-set's any-child member so it is never left a zombie; a refusal stated
fail-loud on `stderr`), `Backspace` goes up); `F2` renames the selected item,
`Ctrl+Shift+N` makes a new folder, `Ctrl+X`/`Ctrl+C`/`Ctrl+V` cut, copy,
and paste the selection (a same-volume move is one `fs_rename`, a
cross-volume move copies-then-deletes, a copy streams in bounded chunks),
`Delete` removes it after a modal confirmation, and `Alt+Enter` shows its
properties — every write the launching user's own permission-checked VFS
call, no new capability, stopping fail-loud on `stderr` at the first
refusal (`AGENTS.md` §2.24, §5.4); a `CloseRequested` from the desktop
ends the program cleanly; every bring-up refusal exits fail-loud with a
reserved code and a stated reason on `stderr`.

## Capabilities

`CAP_CONSOLE_WRITE` (fail-loud diagnostics), `CAP_FS_ACCESS` (its
directory listings — every read still permission-checked per inode
under the launching user's identity), `CAP_SHM` (the granted window
frame region), and `CAP_PROC_SPAWN` (launching an activated `<Name>.app`
bundle through the signed load gate; the child runs as the launching user,
no ambient authority). See `AppInfo.toml`.

## Test surface

The engine's behaviour is exhaustively host-tested in `lib/browse`
(`cargo test -p tairix-browse`); this package carries only the inert
host stub the workspace tooling compiles.
