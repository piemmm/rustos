# tairix-proglib

Stability tier: **experimental**.

The program-library catalog engine: the one definition of the folder-organised
catalog of launchable applications the desktop's Program Library presents —
the closed folder taxonomy (`LibraryCategory`), the validated entry model
(`LibraryEntry` and its `EntryId` / `DisplayName` / `BundlePath` / `IconAsset`
newtypes), the `<id>.<field>` line grammar and its closed key registry
(`name`, `bundle`, `category`, `icon`, and the patch-only `hidden`), the
bounded fail-closed parser, the canonical render, and the one machine ∪
overlay `merge`.

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
taxonomy, a duplicate key, a `hidden` on a declared entry, or a bundle path
that is not an application bundle inside an application store. A half-read
library would silently drop or mis-file an application a user expects to find,
so a reader runs on the empty catalog rather than guessing at a partial
intent, and a writer refuses the edit outright. A hide is final in a merge: a
machine-wide policy that hides an application cannot be undone by an
unprivileged account's own overlay.

The crate performs no I/O and holds no authority: file access goes through the
secured VFS under the caller's own kernel-attested identity, the machine
store's write is gated by the machine-wide settings-write capability, and a
per-user overlay is an ordinary write under that user's own identity.
Launching an entry remains subject to the loader's signature and capability
gate; the bundle-path confinement here is the earlier, cheaper refusal.

`no_std` + `alloc`; host-unit-tested beside the code and fuzzed by
`tests/fuzz_proglib.rs` (registered with `cargo xtask fuzz`). The staged
design is `plans/NEW-TASKBAR.md`; the subsystem page is
`docs/src/lib/proglib.md`.
