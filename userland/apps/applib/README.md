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
catalog document is read and written through the one `lib/proglib`
registry, so this writer and the desktop's readers can never diverge.

## The two layers, and how each is gated

The **machine** store stays an ordinary `/System/Settings` administrator
document (`tairix_proglib::LIBRARY_PATH`). It is machine policy rather than
any one application's data, so its per-inode owner/mode/ACL record admits
or refuses the write kernel-side under the caller's attested identity.

The **overlay** is per-user, per-application data, and every application the
account launched could previously read *and rewrite* it — a hostile program
could file a launcher row named "Terminal" against a bundle of its choosing.
It now lives in this application's **published** app-data scope
(`plans/APPDATA.md` §1.1, `src/store.rs`): `applib` is the only principal
that can write it, because an application publishes only its own scope, and
the desktop session reads it through the one sanctioned foreign-read shape,
which carries no scope field and so cannot name a private document at all.
Nothing spells a path to it — the service derives the store from the
identity the kernel attests — and no home directory is needed to reach it.

The tool holds no authority of its own, and hiding or listing an application
is presentation only: launching stays behind the loader's signature and
capability gate.

`no_std` + `alloc`; no `unsafe`. Host tests live in `src/tests.rs` and
`src/store_tests.rs`, the latter driving the shared fake app-data service; the
bundle's `Help/` tree carries the canonical `en-US` document plus the
required locales, linted by `cargo xtask help-lint`. The subsystem page is
`docs/src/userland/applib.md`.
