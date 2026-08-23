# Program-library admin (`applib`)

`userland/apps/applib` is the command app that administers the desktop's
program library — the folder-organised catalog of launchable applications
the launcher presents (`plans/NEW-TASKBAR.md` T2/T3). It is the
command-line half of the catalog story: the engine that defines the
catalog itself is `lib/proglib` (see [Program-library
catalog](../lib/proglib.md)), and `applib` is a thin, seam-injected client
of it, so the tool, the image builder, and the desktop session can never
disagree about what a store says.

## Commands

- `applib [list [--category <folder>]]` — print the **resolved** library
  (machine store ∪ the caller's overlay, `lib/proglib::merge`), folder by
  folder in taxonomy order; one `  <id>  <name>  <bundle>` line per entry.
  This is exactly the view a launcher shows: hidden entries are absent,
  and the user's own adjustments win.
- `applib add <bundle> [--category <folder>] [--name <n>] [--icon <a>]
  [--user]` — register (or update) a bundle. Identity, display name,
  folder, and icon come from the bundle's own signed `AppInfo` manifest
  (`AppInfoHeader::library_category` / `library_icon`); the switches
  override it. A manifest that declares no listing needs an explicit
  `--category` — the tool never guesses a folder.
- `applib remove <id|bundle> [--user]` — drop a record, by entry
  identifier or by the bundle path it was registered with.
- `applib hide <id> [--user]` / `applib show <id> [--user]` — record a
  visibility verdict: on the target store's own declared entry where it
  has one, else as an overlay patch. A hidden record keeps its identifier
  claimed, so a rescan cannot resurrect what a curator suppressed; the
  overlay's verdict wins at resolve time.
- `applib rescan [--user]` — walk the application stores
  (`/System/Commands` and `/System/Applications`, then `/Apps`; under
  `--user`, the caller's own `<home>/Commands` and `<home>/Applications`),
  read each bundle's manifest, and `lib/proglib::reconcile` every listed
  bundle the catalog does not know yet. Curation is never disturbed; a
  bundle with an unreadable or undecodable manifest is skipped and
  counted, never a reason to abort; an unchanged catalog is not rewritten.

By default the tool edits the machine-wide store
(`/System/Settings/ProgramLibrary/library.conf`); `--user` targets the
caller's own overlay, which lives in this application's **published**
app-data scope. No path names it: the settings service derives the store
from the identity the kernel attests, so the overlay needs no `HOME` and
`applib` is the only principal that can write it. On success the tool is quiet on
stdout; each completed change emits one `stdinfo` summary record on fd 3
(`apps.library_entry_added` / `_removed` / `_hidden` / `_shown` /
`apps.library_rescan`), best-effort and never load-bearing.

## Security

The tool holds no authority of its own, and the two layers are gated by the
principals that own them. The **machine** store is read and written whole
through the secured VFS under the caller's kernel-attested identity: it is a
system-owned file whose per-inode owner/mode/ACL record admits or refuses
the write kernel-side, so an ordinary account reads it and personalises
through its own overlay. The **overlay** is reached over the app-data
service and gated on the bundle identity the kernel attests for this
program, so no other application the user launches can rewrite their library
behind their back — the defect that migration closed
(`plans/APPDATA.md` §1.1). A refused write states its reason and changes
nothing, and an unreachable overlay is a refusal rather than an empty
library, so an edit is never published over settings the tool never saw.
Catalog documents and bundle manifests are untrusted input, read by the
bounded, fail-closed `lib/appconf` engine, `lib/proglib` registry, and
`lib/abi` decoder; a store the registry cannot fully read refuses the whole
operation rather than guessing at a merge. The `rescan` walk is
bounded (`MAX_WALK_DEPTH`, `MAX_WALK_ENTRIES`) and fails closed on a tree
it cannot believe. Cataloguing is presentation, never authority: launching
a bundle stays behind the loader's signature and capability gate
regardless of what the catalog says.

## Testing

The engine is `no_std`, `unsafe`-free, and host-tested beside the code
(`src/tests.rs`): the GNU-style grammar, every operation against in-memory
store/bundle fixtures, every refusal (unknown folder/entry, unlisted
bundle, absent home, malformed store, denied write), the walk bounds, the
curation-preserving rescan, and the advisory records. The bundle's
`Help/` documents (canonical `en-US` plus the required locales) are
checked by the same suite and by `cargo xtask help-lint`.
