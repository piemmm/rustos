# tairix-proglib

Stability tier: **experimental**.

The program-library catalog engine: the one definition of the folder-organised
catalog of launchable applications the desktop's Program Library presents —
the closed folder taxonomy (`LibraryCategory`), the validated entry model
(`LibraryEntry` and its `EntryId` / `DisplayName` / `BundlePath` / `IconAsset`
newtypes), the `<id>.<field>` line grammar and its closed key registry
(`name`, `bundle`, `category`, `icon`, `hidden`), the bounded fail-closed
parser, the canonical render, the one machine ∪ overlay `merge`, and the
`reconcile` fold a discovery rescan registers new bundles through.

The catalog is data on the volume, never a compiled-in table: a machine-wide
store at `/System/Settings/ProgramLibrary/library.conf`
(`MACHINE_LIBRARY_PATH`) plus an optional per-user overlay at
`/Users/<u>/Settings/ProgramLibrary/library.conf` (`user_library_path`). The
command app that edits a catalog, the installer that registers a bundle, and
the session that draws the library all go through this engine, so a writer and
a reader can never disagree about what a catalog says. An absent store means
"no catalogued applications", not an error.

A catalog is untrusted input, including one a hostile or corrupted installer
wrote: the parser is bounded (`MAX_CATALOG_LEN` / `MAX_ENTRIES` /
`MAX_LINE_LEN` and the per-field length caps) and refuses the **whole**
document (`CatalogError`, with the offending line where one is meaningful) on
anything it does not fully understand — an unknown field, a folder outside the
taxonomy, a duplicate key, a malformed flag, or a bundle path that is not an
application bundle inside an application store. A half-read library would
silently drop or mis-file an application a user expects to find, so a reader
runs on the empty catalog rather than guessing at a partial intent, and a
writer refuses the edit outright. Visibility resolves with the overlay's
verdict last: a declaration may suppress itself (`hidden true`, which keeps
its identifier claimed so a rescan cannot resurrect it), and the user's own
patch re-shows or hides it — hiding is presentation, never authority, so the
account that owns the view has the last word on what it sees.

The crate performs no I/O and holds no authority: file access goes through the
secured VFS under the caller's own kernel-attested identity — the machine
store is a system-owned file whose per-inode owner/mode/ACL record admits or
refuses the write, and a per-user overlay is an ordinary write under that
user's own identity. Launching an entry remains subject to the loader's
signature and capability gate; the bundle-path confinement here is the
earlier, cheaper refusal.

`no_std` + `alloc`; host-unit-tested beside the code and fuzzed by
`tests/fuzz_proglib.rs` (registered with `cargo xtask fuzz`). The staged
design is `plans/NEW-TASKBAR.md`; the subsystem page is
`docs/src/lib/proglib.md`.
