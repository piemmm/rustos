# `tairix-proglib` — the program-library catalog engine

`lib/proglib` is the single definition of the folder-organised catalog of
launchable applications the desktop's Program Library presents. The catalog
is **data on the volume**, never a compiled-in table, in two layers: a
machine-wide store at `tairix_proglib::LIBRARY_PATH`
(`/System/Settings/ProgramLibrary/library.conf`) plus an optional per-user
overlay in the library-admin command's **published** app-data scope
(`tairix_proglib::LIBRARY_PUBLISHER` — [the app-data client](./appdata.md)).
This crate owns the folder taxonomy, the validated entry model, the closed
`<id>.<field>` key registry, the fail-closed reading, the canonical render,
and the one machine ∪ overlay merge — so the command app that edits a
catalog, the installer that registers a bundle, and the session that draws
the library can never disagree about what a catalog says.

## The two layers, and why only one of them is in the app-data store

The **machine** store is an ordinary `/System/Settings` administrator
document, beside the machine's network and configuration stores: it is
machine policy rather than any one application's data, every account reads
it, and only a principal that tree's policy admits may rewrite it.

The **overlay** is per-user, per-application data, and every other
application of that account could previously read *and rewrite* it — a
hostile program could file a launcher row named "Terminal" against a bundle
of its choosing. It therefore lives in the app-data store
(`plans/APPDATA.md` §1.1, AD10): `applib` is the only principal that can
write it, because an application publishes only its own scope, and the
desktop session reads it by naming the publisher on a request shape that
carries no scope field and so cannot name a private document at all.

## The document

A catalog is a plain [`lib/appconf`](./appconf.md) `key = value` document —
the one format engine the app-data store speaks — so this crate defines the
*registry* over it and no grammar, comment rule, or length bound of its own.
Every setting is one field of one record: `<id>.<field> = value`. The key is
split at its **last** `.`, so a reverse-DNS bundle identifier
(`org.tairix.files.name = Files`) is deliberately valid — no field name
contains a `.`. An identifier is judged by the one grammar every consumer of
a bundle identifier applies (`tairix_abi::appinfo::validate_bundle_id`),
which is inside the key grammar, so `<id>.<field>` is always a key a
document can hold.

All the settings sharing an id form one `Record`:

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
hostile or corrupted installer wrote. The document's length, line, key and
value bounds are the format engine's; on top of them `load` bounds the
record count (`MAX_ENTRIES` — derived as the engine's setting bound divided
by what one record costs at its widest, so a catalog at the bound always fits
one document and the render never has to drop a record) and the per-field
lengths, validates every
field through the model's own validators, and refuses the **whole**
document (`CatalogError`) on anything it does not fully understand: a line
the grammar did not read as a setting, an unknown field, a folder outside
the taxonomy, a malformed flag, or a bundle path that is not an application
bundle inside an application store.

That is the opposite of a *settings* registry, where one bad value costs
only its own field, and deliberately so: a catalog is a list, and a
half-read library would silently drop or mis-file an application a user
expects to find, with no field left standing to say anything is missing. A
reader that cannot fully read a store runs on the empty catalog rather than
guessing at a partial intent, and a writer refuses the edit outright.

The bundle-path confinement is the earlier, cheaper refusal — launching an
entry remains subject to the loader's signature and capability gate. The
engine performs no I/O and holds no authority: the machine store is read and
written through the secured VFS under the caller's own kernel-attested
identity, so its per-inode owner/mode/ACL record admits or refuses the write
(an ordinary account reads it but cannot rewrite it), and the overlay is
reached only through the app-data service, gated on the writer's attested
bundle identity.

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

- `load(&Document) -> Result<Catalog, CatalogError>` — the fail-closed
  reading; `document(&Catalog) -> Document` — the canonical render (one
  setting per set field, in identifier + registry order), so
  render→read round-trips exactly.
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
- `LIBRARY_DIR` / `LIBRARY_FILE` / `LIBRARY_PATH` — the machine layer's
  path spellings, and `LIBRARY_PUBLISHER` the overlay's publisher
  identifier, each defined once here.

The crate is `no_std` + `alloc`, performs no I/O, holds no authority, is
host-unit-tested beside the code, and is fuzzed by `tests/fuzz_proglib.rs`
(registered with `cargo xtask fuzz`). Stability tier: experimental
(`lib/proglib/README.md`). The staged design is `plans/NEW-TASKBAR.md`.
