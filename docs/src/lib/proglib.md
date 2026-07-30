# `tairix-proglib` — the program-library catalog engine

`lib/proglib` is the single definition of the folder-organised catalog of
launchable applications the desktop's Program Library presents. The catalog
is **data on the volume**, never a compiled-in table: a machine-wide store
at `tairix_proglib::MACHINE_LIBRARY_PATH`
(`/System/Settings/ProgramLibrary/library.conf`) plus an optional per-user
overlay at `tairix_proglib::user_library_path(user)`
(`/Users/<u>/Settings/ProgramLibrary/library.conf`). This crate owns the
folder taxonomy, the validated entry model, the line grammar and its closed
key registry, the bounded fail-closed parser, the canonical render, and the
one machine ∪ overlay merge — so the command app that edits a catalog, the
installer that registers a bundle, and the session that draws the library
can never disagree about what a catalog says.

## The store

One text document per store: `<id>.<field> value` settings, one per line;
`#` begins a comment to end of line; blank and comment-only lines carry no
setting; every `(id, field)` pair appears at most once. The key is split at
its **last** `.`, so a reverse-DNS bundle identifier
(`org.tairix.files.name Files`) is deliberately valid — no field name
contains a `.`.

All the lines sharing an id form one `Record`:

- a **declared entry** (`Record::Entry`, a complete `LibraryEntry`) where
  the record names both a `bundle` and a `name`;
- a **patch** (`Record::Patch`, an `EntryPatch`) where it does not — a
  rename, re-file, re-icon, or hide of an entry the document does not own.

An **absent** store is not an error: it means "no catalogued applications"
(`Catalog::default`), which is how a fresh installation behaves before the
first discovery pass registers the shipped bundles.

## The registry

Every field is drawn from the closed `EntryKey` set, in render order:

| Field      | Value                                   | Meaning |
|------------|-----------------------------------------|---------|
| `name`     | bounded display text                    | the name a launcher shows |
| `bundle`   | absolute `.app` path in an app store    | the bundle the entry launches |
| `category` | a `LibraryCategory` id                  | the folder the entry is filed under |
| `icon`     | an asset id in the bundle's `Resources/`| the entry's icon |
| `hidden`   | `true` \| `false`                       | hide the entry (patches only) |

`hidden` is a patch-only field: a document that declares an entry states it
by declaring it, so hiding what you yourself declare is a contradiction
rather than a setting, and is refused. Adding a field means adding an
`EntryKey` variant plus its parse/render arms in the same change; there is
no free-form key namespace and no second store.

`LibraryCategory` is the closed, curated folder taxonomy (`Accessories`,
`Graphics`, `Internet`, `Multimedia`, `Office`, `Programming`, `Games`,
`SystemTools`, `Utilities`, `Other`) with locale-neutral ids and a total,
deterministic presentation order. A folder is a *view* over the entries
filed under a category — `Catalog::folder` returns one category's entries in
a stable order and `Catalog::folders` the non-empty categories in taxonomy
order — never a second directory tree on disk.

## Security

A catalog is **untrusted input** to every consumer, including a store a
hostile or corrupted installer wrote. The parser is bounded
(`MAX_CATALOG_LEN`, `MAX_ENTRIES`, `MAX_LINE_LEN`, and the per-field length
caps), validates every field through the model's own validators, and refuses
the **whole** document (`CatalogError`, carrying the offending 1-based line
where one is meaningful) on anything it does not fully understand: an
unknown field, a folder outside the taxonomy, a duplicate key, a
`hidden` on a declared entry, or a bundle path that is not an application
bundle inside an application store. A half-read library would silently drop
or mis-file an application a user expects to find, so a reader that cannot
fully parse a store runs on the empty catalog rather than guessing at a
partial intent, and a writer refuses the edit outright.

The bundle-path confinement is the earlier, cheaper refusal — launching an
entry remains subject to the loader's signature and capability gate. The
engine performs no I/O and holds no authority: reading and writing the
documents goes through the secured VFS under the caller's own
kernel-attested identity, the machine store's write is gated by the
machine-wide settings-write capability, and a per-user overlay is an
ordinary write under that user's own identity.

## Merging machine and overlay

`merge(machine, overlay)` is the one pure overlay resolution, in order:

1. the machine catalog's declared entries;
2. the overlay's declared entries, which replace a machine entry of the same
   identifier outright (a user's own bundle wins over one it shadows);
3. the machine's patches, then the overlay's, so a user's rename wins over a
   machine-wide one;
4. an entry any patch hides is dropped — a hide is **final**, so a
   machine-wide policy that hides an application cannot be undone by an
   unprivileged account's own overlay.

A patch naming no entry is discarded rather than refused: its bundle has
been removed or was never installed, an ordinary state of the world — and
because the patch stays in the user's own document, re-installing the
application restores the personalisation. The result declares entries only,
holds at most one record per identifier, and therefore can never exceed
`MAX_ENTRIES`; applying a patch cannot fail, so neither can the merge.

## API shape

- `parse(&str) -> Result<Catalog, CatalogError>` — the bounded, fail-closed,
  line-numbered parse.
- `render(&Catalog) -> String` — the canonical document (one line per set
  field, in identifier + registry order), so render→parse round-trips
  exactly.
- `Catalog::{insert, patch, remove, get, entry, entry_patch, records,
  entries, patches, folder, folders, len, is_empty}` — the store operations;
  `insert`/`patch` fail closed with `CatalogFull` at `MAX_ENTRIES` rather
  than growing without bound.
- `LibraryEntry` and the validated newtypes `EntryId` / `DisplayName` /
  `BundlePath` / `IconAsset` (`EntryError` on refusal), so an invalid entry
  is unrepresentable.
- `EntryPatch` — the overlay adjustment (`set_name`/`set_category`/
  `set_icon`/`set_hidden`).
- `LibraryCategory::{ALL, as_str, from_id}` — the closed taxonomy.
- `EntryKey::{ALL, as_str, from_id}` — the closed field registry.
- `LIBRARY_DIR` / `LIBRARY_FILE` / `MACHINE_LIBRARY_PATH` /
  `user_library_path` — the path spellings, defined once here.

The crate is `no_std` + `alloc`, performs no I/O, holds no authority, is
host-unit-tested beside the code, and is fuzzed by `tests/fuzz_proglib.rs`
(registered with `cargo xtask fuzz`). Stability tier: experimental
(`lib/proglib/README.md`). The staged design is `plans/NEW-TASKBAR.md`.
