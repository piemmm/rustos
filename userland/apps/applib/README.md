# tairix-applib

The `applib` command app: administer the desktop's program library — the
folder-organised catalog of launchable applications the launcher presents
(`plans/NEW-TASKBAR.md` T2/T3).

`applib` lists the resolved library (machine store ∪ the caller's overlay,
exactly what the launcher shows); `add`/`remove` register and unregister a
bundle, deriving name/folder/icon from the bundle's own signed `AppInfo`
manifest unless overridden; `hide`/`show` record a visibility verdict; and
`rescan` walks the application stores and registers every listed bundle the
catalog does not know yet — discovery from the bundles on disk, never a
compiled-in list. `--user` targets the caller's own overlay instead of the
machine-wide store.

The crate is the pure, host-testable engine (`src/lib.rs`: the GNU-style
grammar and the operations over injected `Store`/`Bundles`/`Output` seams)
plus the freestanding `Run` binary (`src/run.rs`) that wires the secured
VFS, the inherited standard streams (listings on fd 1, `stdinfo` advisory
records on fd 3), and the shared `lib/help` short-help engine. Every
catalog document is read and written through the one `lib/proglib` engine,
so this writer and the desktop's readers can never diverge.

The tool holds no authority of its own: the machine store is a
system-owned file whose per-inode owner/mode/ACL record admits or refuses
the write kernel-side under the caller's attested identity, and hiding or
listing an application is presentation only — launching stays behind the
loader's signature and capability gate.

`no_std` + `alloc`; no `unsafe`. Host tests live in `src/tests.rs`; the
bundle's `Help/` tree carries the canonical `en-US` document plus the
required locales, linted by `cargo xtask help-lint`. The subsystem page is
`docs/src/userland/applib.md`.
