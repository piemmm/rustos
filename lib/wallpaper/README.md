# tairix-wallpaper

Stability tier: **experimental**.

The desktop's user-scope settings engine: the per-user **desktop settings
document** (the backdrop keys — wallpaper choice, fit, backdrop colour, icon
flow, sort order — and the appearance keys — light/dark, contrast, density,
motion, interface scale), the shipped default wallpaper catalog and its
bounded fail-closed listing model, the one pure wallpaper-placement geometry
the desktop renderer and the chooser's preview both draw through, and the one
client every surface asks the session to adopt a change with. It defines the
validated settings model (`DesktopSettings`) and the closed key registry over
the store's document (`SettingsKey`) — plus the shipped wallpaper identity
(`WALLPAPER_STORE`, `DEFAULT_WALLPAPER`), the placement geometry (`place`,
`decode_request`), and the apply client (`ApplyOutcome`, and `apply` behind
the `rt` feature).

## Where the document lives, and who may touch it

In the desktop session's **published** app-data scope
(`plans/APPDATA.md` §3.11) — not at a path any program spells. Two
properties follow from the store rather than from convention:

- The session is the only **writer**. An application publishes only its
  own scope, so no other program the user launches — including the chooser
  (`wallpaper.app`) — can write the desktop's document at all. The chooser
  *asks* over the pinboard channel and the session decides.
- Any application may **read** it, by naming `PINBOARD_PUBLISHER` on a
  request shape that carries no scope field, so "read the desktop's private
  settings" is not a request that exists.

That replaces `/Users/<u>/Settings/Pinboard/pinboard.conf`, which every
application of that user could read *and rewrite*. An absent store means
"the documented defaults", not an error. Pinboard settings are per-user
state only; there is no machine-wide store, and the published scope has no
layer beneath it, so nobody can make the desktop appear to say something it
never said.

## The registry

The document is a plain `lib/appconf` `key = value` document — the one
format engine the app-data store speaks — so this crate defines the
registry over it and no grammar of its own. Every value is drawn from its
key's own closed vocabulary:

| Key         | Value                                              | Default                                       |
|-------------|----------------------------------------------------|-----------------------------------------------|
| `wallpaper` | `none`, or an absolute path to an image            | `/System/Graphics/Wallpapers/TAIRiX/tairix-dark.jpg` |
| `fit`       | `fill` \| `fit` \| `stretch` \| `centre` \| `tile` | `fill`                                        |
| `backdrop`  | `theme`, or six bare hex digits `rrggbb`           | `theme`                                       |
| `icons`     | `leading` \| `trailing`                            | `leading`                                     |
| `sort`      | `name` \| `kind` \| `size` \| `date`               | `name`                                        |
| `appearance`| `dark` \| `light`                                  | `dark`                                        |
| `contrast`  | `normal` \| `high` \| `monochrome`                 | `normal`                                      |
| `density`   | `compact` \| `normal` \| `comfortable`             | `normal`                                      |
| `motion`    | `full` \| `reduced`                                | `full`                                        |
| `scale`     | a bare decimal percentage in `Scale`'s own range   | `100`                                         |

`SettingsKey::PINBOARD` and `SettingsKey::APPEARANCE` are the two groups: the
backdrop and the icons on it, and how every surface is drawn. They share one
document because they share one owner and one published scope. The four
appearance value sets are `tairix_abi::desktop`'s own, imported rather than
restated, because the session publishes them to every application over the
window channel.

A colour is written **bare** — `112233`, never `#112233`. That is now a
*registry* rule rather than a grammar one: the format engine quotes a value
carrying a `#` and round-trips it perfectly well, so the crate keeps one
spelling of a colour because two would be two ways for consumers to
disagree about whether they mean the same backdrop. [`Rgb::from_hex`] reads
bare digits and [`Rgb::to_hex`] writes them. A wallpaper *path* carrying a
`#` is accepted, because the path grammar is now the only thing judging it.

## Two readings, deliberately different

`DesktopSettings::load` is the **tolerant** one, for a document held in a
store: a value the registry refuses leaves that one field at its documented
default and is *named* to the caller, so one stale setting costs only
itself and never blanks a user's desktop. It reads through
`tairix_appconf::Lookup`, so the same loader serves the session's own
published-scope handle and the `Document` a foreign read answers with.

`merge` is the **strict** one, for a document that arrived over the
pinboard channel: a line outside the grammar, a key outside the registry,
or a value outside a key's closed set is a defect in the *sender* rather
than something a person typed, and adopting a desktop the sender did not
describe is worse than refusing it (`DocumentRefusal` names which). It
merges over what the desktop already holds rather than replacing it, so a
surface that renders only the keys it edits cannot reset a setting it never
showed — and refuses whole, leaving the base untouched.

`DesktopSettings::document` renders the canonical form both readings
accept: every registry key, in registry order, including one still at its
default, so a render/read round trip is exact. That is what the session
persists; a surface *asking* for a change renders one group with
`document_of`.

A settings document is untrusted input either way: the format engine bounds
the document, the line, the key and the value, and `MAX_WALLPAPER_PATH_LEN`
bounds the one value that carries a path.

The shipped wallpaper masters ship read-only at `WALLPAPER_STORE`
(`/System/Graphics/Wallpapers`), filed one directory level deep in
**categories** (`Space`, `Nature`, `City`, `Abstract`, `TAIRiX`) and
discovered at build time from `lib/wallpaper/assets/` by `tools/syshelp` —
never a hand-maintained list. A category's directory name *is* the label a
chooser draws, so adding a category is authoring a directory and there is no
name → label table to drift. `catalog_categories` filters and orders a
listing of the store's own subdirectories exactly as `catalog_entries` does a
listing of one category's files.
Each master is authored no larger than `lib/sandbox`'s
`MAX_DESTINATION_WIDTH`×`MAX_DESTINATION_HEIGHT` (3840×2160): JPEG entropy
decoding cannot skip blocks, so a source pixel beyond what the renderer
will ever draw costs decode time no screen can use. `catalog_entries` is
the one bounded, fail-closed definition of which files in a directory
listing a chooser may offer: it performs no I/O of its own, filtering to
the decodable extensions, rejecting illegal names, skipping oversized
files, and capping and sorting the result.

The fit geometry (`place`, `decode_request`) is pure arithmetic with no
rendering of its own: given a source image size, a screen size, and a
`WallpaperFit`, it answers the destination rectangle, the sampled source
rectangle, and whether the source tiles — every dimension checked/widened
through `u64` so it never panics and never divides by zero, however extreme
the aspect ratio.

The registry, the catalog and the fit geometry perform no I/O and hold no
authority; the `rt`-gated apply client makes exactly one call, asking the
session to adopt a document it cannot itself write. Reading and writing the
document, and listing a wallpaper directory, go through the secured VFS
under the caller's own kernel-attested identity — a per-user store is an
ordinary write under that user's own identity. A wallpaper path surviving
this crate's validation still names untrusted image content; decoding it
happens only inside the parser sandbox (`lib/sandbox`), never here.

`no_std` + `alloc`; host-unit-tested beside the code and fuzzed by
`tests/fuzz_wallpaper_settings.rs`. The staged design is
`plans/PINBOARD.md`; the subsystem page is `docs/src/lib/wallpaper.md`.
