# tairix-proglib

Stability tier: **experimental**.

The program-library catalog engine: the one definition of the folder-organised
catalog of launchable applications the desktop's Program Library presents —
the closed folder taxonomy (`LibraryCategory`), the validated entry model
(`LibraryEntry` and its `EntryId` / `DisplayName` / `BundlePath` / `IconAsset`
newtypes), the closed `<id>.<field>` key registry (`name`, `bundle`,
`category`, `icon`, `hidden`), the fail-closed reading (`load`), the
canonical render (`document`), the one machine ∪ overlay `merge`, and the
`reconcile` fold a discovery rescan registers new bundles through.

A catalog is a plain `lib/appconf` `key = value` document — the one format
engine the app-data store speaks — so this crate defines the *registry* over
it and no grammar, comment rule, or length bound of its own.

## The two layers

The catalog is data on the volume, never a compiled-in table, in two layers.
The **machine** store at `/System/Settings/ProgramLibrary/library.conf`
(`LIBRARY_PATH`) is an ordinary administrator document beside the machine's
network and configuration stores: machine policy rather than any one
application's data, read by every account and rewritable only by a principal
that tree's policy admits.

The **overlay** is per-user, per-application data, and every other
application of that account could previously read *and rewrite* it — a
hostile program could file a launcher row named "Terminal" against a bundle
of its choosing. It lives in the library-admin command's **published**
app-data scope (`LIBRARY_PUBLISHER`, `plans/APPDATA.md` §1.1): `applib` is
the only principal that can write it, and the desktop session reads it
through the one sanctioned foreign-read shape, which carries no scope field
and so cannot name a private document at all.

The command app that edits a catalog, the installer that registers a bundle,
and the session that draws the library all go through this engine, so a
writer and a reader can never disagree about what a catalog says. An absent
store means "no catalogued applications", not an error.

A catalog is untrusted input, including one a hostile or corrupted installer
wrote. The document's length, line, key and value bounds are the format
engine's; on top of them `load` bounds the record count (`MAX_ENTRIES`,
derived as the engine's setting bound divided by what one record costs at its
widest, so a catalog at the bound always fits one document) and the per-field
lengths,
and refuses the **whole** document (`CatalogError`) on anything it does not
fully understand — a line the grammar did not read as a setting, an unknown
field, a folder outside the taxonomy, a malformed flag, or a bundle path that
is not an application bundle inside an application store. That is the
opposite of a *settings* registry, where one bad value costs only its own
field, and deliberately so: a half-read library would silently drop or
mis-file an application a user expects to find, with no field left standing
to say anything is missing. A reader runs on the empty catalog rather than
guessing at a partial intent, and a writer refuses the edit outright.
Visibility resolves with the overlay's
verdict last: a declaration may suppress itself (`hidden true`, which keeps
its identifier claimed so a rescan cannot resurrect it), and the user's own
patch re-shows or hides it — hiding is presentation, never authority, so the
account that owns the view has the last word on what it sees.

The crate performs no I/O and holds no authority: the machine store is read
and written through the secured VFS under the caller's own kernel-attested
identity, so its per-inode owner/mode/ACL record admits or refuses the write,
and the overlay is reached only through the app-data service, gated on the
writer's attested bundle identity. Launching an entry remains subject to the loader's
signature and capability gate; the bundle-path confinement here is the
earlier, cheaper refusal.

`no_std` + `alloc`; host-unit-tested beside the code and fuzzed by
`tests/fuzz_proglib.rs` (registered with `cargo xtask fuzz`). The staged
design is `plans/NEW-TASKBAR.md`; the subsystem page is
`docs/src/lib/proglib.md`.
