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
  rename, re-file, re-icon, or visibility verdict on an entry the document
  does not own.

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
| `hidden`   | `true` \| `false`                       | the visibility verdict |

`hidden` is legal on either record shape. On a declared entry it is the
curator's own suppression: the record stays — claiming its identifier, so a
discovery rescan cannot resurrect what was deliberately hidden — but the
resolved catalog a launcher draws drops it. In a patch it is the overlay's
verdict on an entry declared elsewhere: `true` hides it, `false` re-shows
what the layer below hid. Visible is the default, so the canonical render
emits `hidden` only as `true` on a declaration and only as set in a patch.
Adding a field means adding an `EntryKey` variant plus its parse/render
arms in the same change; there is no free-form key namespace and no second
store.

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
unknown field, a folder outside the taxonomy, a duplicate key, a malformed
flag, or a bundle path that is not an application bundle inside an
application store. A half-read library would silently drop
or mis-file an application a user expects to find, so a reader that cannot
fully parse a store runs on the empty catalog rather than guessing at a
partial intent, and a writer refuses the edit outright.

The bundle-path confinement is the earlier, cheaper refusal — launching an
entry remains subject to the loader's signature and capability gate. The
engine performs no I/O and holds no authority: reading and writing the
documents goes through the secured VFS under the caller's own
kernel-attested identity: the machine store is a system-owned file whose
per-inode owner/mode/ACL record admits or refuses the write (an ordinary
account reads it but cannot rewrite it), and a per-user overlay is an
ordinary write under that user's own identity.

## Merging machine and overlay

`merge(machine, overlay)` is the one pure overlay resolution, in order:

1. the machine catalog's declared entries;
2. the overlay's declared entries, which replace a machine entry of the same
   identifier outright (a user's own bundle wins over one it shadows);
3. the machine's patches, then the overlay's, applied field by field — so
   a user's adjustment wins over a machine-wide one, the visibility verdict
   included: a user's overlay re-shows an application the machine store
   declared hidden, and hides one it shows;
4. an entry whose resolved verdict is hidden is dropped from the result.
   Its record stays in the document that declared it, so its identifier
   remains claimed and a discovery rescan cannot resurrect what a curator
   suppressed.

Hiding is presentation, never authority — launching a bundle stays behind
the loader's signature and capability gate either way — so the account that
owns the view has the last word on what it sees.

A patch naming no entry is discarded rather than refused: its bundle has
been removed or was never installed, an ordinary state of the world — and
because the patch stays in the user's own document, re-installing the
application restores the personalisation. The result declares visible
entries only, holds at most one record per identifier, and therefore can
never exceed `MAX_ENTRIES`; applying a patch cannot fail, so neither can
the merge.

## Discovery reconciliation

`Catalog::reconcile(discovered)` is the self-healing fold a rescan uses:
every discovered entry whose identifier no existing record claims is
declared, and every existing record — a curated entry, a hidden
suppression, or a patch — stands untouched, so re-running discovery
registers what an installer missed without disturbing curation. Within one
fold the first entry under an identifier wins, so the caller's
deterministic scan order decides. The fold is refused whole (`CatalogFull`)
if it would exceed `MAX_ENTRIES`, leaving the catalog unchanged.

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
- `Catalog::reconcile` — the discovery fold above; `merge` — the machine ∪
  overlay resolution.
- `LibraryEntry` and the validated newtypes `EntryId` / `DisplayName` /
  `BundlePath` / `IconAsset` (`EntryError` on refusal), so an invalid entry
  is unrepresentable; `hidden`/`set_hidden` carry a declaration's own
  suppression.
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
