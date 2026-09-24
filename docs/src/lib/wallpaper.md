# `tairix-wallpaper` — the desktop settings document and wallpaper engine

`lib/wallpaper` is the shared engine behind the desktop's own user-scope
configuration: the per-user **desktop settings document**, the shipped
wallpaper set and the listing model a gallery draws its thumbnail grid from,
the one wallpaper placement geometry the desktop renderer and every preview
draw through, and the one client every surface asks the session to adopt a
change with. The settings are **data on the volume**, never a compiled-in table:
one document per user, in the desktop session's **published** app-data scope
([the app-data store](./appdata.md), `plans/APPDATA.md` §3.11).
Because there is exactly one definition of the registry, of the catalog, and
of the geometry, no two consumers can disagree about what the settings say,
which wallpapers exist, or how a fit places one.

## The store, and who may touch it

The document is a plain `lib/appconf` `key = value` document — the one format
engine the app-data store speaks — so this crate defines the closed
*registry* over it and no grammar of its own. It is reached only through the
app-data service; no program spells a path to it. Two properties follow from
the store rather than from convention:

- **The session is the only writer.** An application publishes only its own
  scope, so no other program the user launches — including the Settings
  application, where the desktop picture is chosen — can write the desktop's
  document at all. Settings asks over the pinboard channel and the session
  decides.
- **Any application may read it**, by naming `PINBOARD_PUBLISHER` on a
  request shape that carries no scope field, so "read the desktop's private
  settings" is not a request that exists. That is the sanctioned sharing
  channel replacing `/Users/<u>/Settings/Pinboard/pinboard.conf`, which every
  application of that user could also *rewrite*.

An **absent** store is not an error: it means the documented defaults
(`DesktopSettings::default`), and so does an account whose session has never
run. A document naming only some keys leaves the rest at their default.
Pinboard settings are per-user state only; there is no machine-wide store,
and the published scope has no layer beneath it, so nobody can make the
desktop appear to say something it never said.

## Two readings, deliberately different

`DesktopSettings::load` is the **tolerant** one, for a document held in a
store: a value the registry refuses leaves that one field at its documented
default and is *named* to the caller, so one stale setting costs only itself
and never blanks a user's desktop. It reads through `tairix_appconf::Lookup`,
so the same loader serves the session's own published-scope handle and the
`Document` a foreign read answers with.

`merge` is the **strict** one, for a document that arrived over the pinboard
channel: a line outside the grammar, a key outside the registry, or a value
outside a key's closed set is a defect in the *sender* rather than something
a person typed, and adopting a desktop the sender did not describe is worse
than refusing it. `DocumentRefusal` names which. It refuses whole — the merge
runs on a copy, so a refusal partway through leaves the base untouched.

It **merges** rather than replaces, because the desktop has more than one
surface asking it to change and no surface shows every setting: the backdrop
menu and Settings' Wallpaper pane edit the pinboard keys, its Appearance and
Accessibility panes edit the appearance keys. A key a sender did not name
keeps the value the desktop already has, so one surface cannot undo the
other's change by staying silent about it — which taking the absent keys as
their *defaults* would do on every single apply.

`DesktopSettings::document` renders the canonical form both readings accept:
every registry key, in registry order, so a render/read round trip is exact.
That is what the session *persists*. A surface *asking* for a change renders
only the keys it edits, with `document_of` over `SettingsKey::PINBOARD` or
`SettingsKey::APPEARANCE`.

## The registry

Every line is drawn from the closed `SettingsKey` set, and every value from
that key's own closed vocabulary:

| Key         | Value                                             | Default                                       |
|-------------|---------------------------------------------------|-----------------------------------------------|
| `wallpaper` | `none`, or an absolute path to an image           | `/System/Graphics/Wallpapers/Nature/sandstone.jpg` |
| `fit`       | `fill` \| `fit` \| `stretch` \| `centre` \| `tile`| `fill`                                        |
| `backdrop`  | `theme`, or six bare hex digits `rrggbb`          | `theme`                                       |
| `icons`     | `leading` \| `trailing`                           | `leading`                                     |
| `sort`      | `name` \| `kind` \| `size` \| `date`              | `name`                                        |
| `appearance`| `dark` \| `light`                                 | `dark`                                        |
| `contrast`  | `normal` \| `high` \| `monochrome`                | `normal`                                      |
| `density`   | `compact` \| `normal` \| `comfortable`            | `normal`                                      |
| `motion`    | `full` \| `reduced`                               | `full`                                        |
| `scale`     | a bare decimal percentage in `Scale`'s own range  | `100`                                         |
| `cursor.set`| a cursor-set name (a plain leaf name within `CURSOR_SET_NAME_MAX`) | `Standard`                   |
| `cursor.size`| `normal` \| `large` \| `larger` \| `largest`     | `normal`                                      |
| `notify.enabled` | `true` \| `false` (the format engine also reads `on` \| `off`) | `true`                  |
| `notify.sources` | `<bundle-id>:<level>` entries, one space apart, in identity order; levels `warning` \| `critical` \| `none` | empty |
| `pointer.primary` | `left` \| `right`                              | `left`                                        |
| `pointer.double_click_ms` | whole milliseconds within `DOUBLE_CLICK_MIN..=DOUBLE_CLICK_MAX` | `500`            |
| `pointer.speed` | a bare decimal percentage, `25..=400`            | `100`                                         |
| `key.repeat_delay_ms` | whole milliseconds, `100..=2000`           | `500`                                         |
| `key.repeat_rate` | `off`, or repeats a second, `1..=60`           | `30`                                          |
| `screensaver.after_min` | `never`, or whole minutes, `1..=1440`    | `10`                                          |
| `screensaver.kind` | `blank` \| `dim` \| `slideshow`               | `blank`                                       |
| `lock.after_min` | `never`, or whole minutes, `1..=1440`           | `15`                                          |

Keys and values are case-sensitive: each has one canonical spelling.

The keys fall into groups, which is a reader's distinction rather than the
document's, and each surface posts only its own: `SettingsKey::PINBOARD` (the
backdrop and the icons standing on it), `APPEARANCE` (how every surface is
drawn), `NOTIFICATIONS`, `POINTER`, `KEYBOARD`, `SCREENSAVER` and `LOCK`.
They share one document because they share one owner and one published scope,
and a desktop half-adopted from several documents is a desktop nobody chose.

A notification source is the kernel-attested bundle identity of the program
that posted, never a name it gave itself; a source with no entry shows
everything, and an entry at `all` is not a spelling at all — its absence is.
`NotifyPolicy::set_level` refuses a change whose spelling would outgrow one
settings value. Every span is a `Duration64` in memory and on every wire;
milliseconds and minutes are the document's spelling, for the person who edits
it.

The four appearance value sets are `tairix_abi::desktop`'s own
(`Appearance`, `Contrast`, `Density`, `Motion`), imported rather than
restated: the session publishes them to every application over the window
channel, so the value this document stores and the byte on that wire are one
definition. `scale` is validated by `tairix_geometry::Scale`, the one
validator of a UI scale, so a percentage this registry accepts is always one
the desktop can actually be drawn at. `cursor.set` holds a
`tairix_theme::CursorSetId` for the same reason — the value this document
stores and the set the compositor activates are one type.

**`cursor.set` names a set; it does not assert one exists.** A name no set
could carry (a separator, an over-long name) is refused here, because it
would be spliced into a store path. Whether a *registered* set answers to it
is the desktop's question: a stored choice outlives the image that shipped
it, so a set an update removed falls back to the built-in one at activation
rather than costing the reader every other key in their document.

A colour is written as **bare** hex digits — `112233`, never `#112233`. The
document's own comment grammar cuts a line at the first `#`, so a
`#`-prefixed colour would be truncated away before any colour parser saw it.
There is therefore exactly one spelling of a colour in the crate:
`Rgb::from_hex` reads bare digits and `Rgb::to_hex` writes them, so a
consumer cannot pick a spelling the document cannot hold.

`render` always emits **every** key in `SettingsKey::ALL` order, including a
key still at its default, so the document a user opens always shows the whole
registry and `parse(render(s)) == s` exactly. Adding a key means adding a
`SettingsKey` variant, its `DesktopSettings` field, and its parse/render
arms in the same change; there is no free-form key namespace and no second
store.

## Asking the session to adopt a change

`apply` (behind the crate's `rt` feature) is the one client of the pinboard
rendezvous, shared by every surface that edits the desktop's settings — the
backdrop menu's pinboard keys and the Settings application's panes. A second copy of the round trip would be two places for "what did the
session say" to drift apart. `ApplyOutcome` distinguishes an adopted change,
a typed refusal with its reason, and a rendezvous nobody answered.

The feature is off by default, so the registry, the catalog and the fit
geometry stay linkable with no runtime behind them — which is what lets the
session's own engine and every host test drive them directly.

## Security

A desktop settings document is **untrusted input** to every consumer, and the
two readings above bound it the same way: the format engine bounds the
document, the line, the key and the value, and `MAX_WALLPAPER_PATH_LEN` bounds
the one value that carries a path. Neither reading ever half-applies a
document — `merge` refuses the whole thing, `load` leaves the refused field
at its documented default and names it — so a desktop is never left in a state
no user asked for.

A `wallpaper` value is validated as a canonical absolute session-view path
(`WallpaperPath`): an empty, relative, alias- or volume-id-rooted,
embedded-control-character, or over-long path is refused, never "fixed up". A
`#` is *not* refused: the format engine quotes such a value and round-trips it
exactly, so a file the user really named `sunset#2.png` is choosable, and the
path grammar is the only thing judging a path. (A backdrop *colour* still has
exactly one bare `rrggbb` spelling — that is a registry rule, keeping one
spelling per colour, not a grammar limitation.) Surviving validation still
means the path names untrusted **content** — the session reads it under its
own identity and the image decoder sniffs and bounds it in its own sandbox
before a pixel is drawn. This crate decodes nothing.

The bounds are fixed validation limits on untrusted input, not growable
capacities: the format engine's `MAX_DOCUMENT_LEN` / `MAX_VALUE_LEN`,
`MAX_WALLPAPER_PATH_LEN` (1 KiB path, held inside `MAX_VALUE_LEN` by a
compile-time assertion), `MAX_WALLPAPER_BYTES` (8 MiB per wallpaper file),
`MAX_WALLPAPER_CATALOG_ENTRIES` (256 offered wallpapers), and
`MAX_WALLPAPER_CATEGORIES` (64 offered categories).

The engine performs no I/O and holds no authority: the document is read and
written through the app-data service under the caller's own kernel-attested
identity, and listing a wallpaper directory goes through the secured VFS.

## The shipped set and the catalog

The OS ships its wallpaper masters read-only under `WALLPAPER_STORE`
(`/System/Graphics/Wallpapers`), filed one directory level deep in
**categories** — `Abstract`, `City`, `Nature`, `Space`, `TAIRiX` — and
discovered at build time from `lib/wallpaper/assets/<Category>/` by
`tools/syshelp`, planted by the image builder; never a hand-maintained list.
A category's directory name *is* the label a gallery draws, so adding a
category is authoring a directory and no name → label table can drift out of
step. `DEFAULT_WALLPAPER_CATEGORY` and `DEFAULT_WALLPAPER` name the default
master's category and file, `category_path(category)` and
`wallpaper_path(category, file)` spell a category and a master, and
`default_wallpaper_path()` spells the default's absolute path, which is also
the default `wallpaper` setting.

`catalog_categories` and `catalog_entries` are the one definition of which
directories and which files a gallery may offer. Neither performs **any**
I/O — the caller lists the store's subdirectories, or one category's files,
and passes the names in. `catalog_entries` admits an entry only when its name
is a legal plain file name (no path separator, no control character, not
`.`/`..`), its extension is one of `.jpg`, `.jpeg`, `.png`
(case-insensitively), and its size is at most `MAX_WALLPAPER_BYTES`;
`catalog_categories` admits a name on the leaf-name rule alone, since a
category carries no extension and no case convention. Anything else is
silently dropped, so a store holding a stray file beside its categories, or a
category mixing wallpapers with unrelated files, yields only what a gallery
can offer rather than a refusal of the whole listing. Both results are sorted
by name and capped — at `MAX_WALLPAPER_CATALOG_ENTRIES` and
`MAX_WALLPAPER_CATEGORIES`.

`desktop_catalog` flattens a whole store walk into the one list the desktop
offers: each category's entries, category by category, in walk order, capped
at `MAX_WALLPAPER_CATALOG_ENTRIES` **in total** because that bound is the
gallery's rather than one directory's. A `CatalogItem` names its category
and its file rather than carrying a path, and `CatalogItem::path` is the one
spelling that turns the two into one — so the picture a gallery shows and
the settings document choosing it cannot disagree about where it lives.

`is_wallpaper_file_name` and `is_wallpaper_category_name` are those name
contracts on their own, so `tools/syshelp`'s build-time discovery applies
exactly the definitions the runtime applies: it walks one category level, and
a master the desktop could never offer, one over the byte bound, a stray file
at the store root, or an illegal category name fails the **build** rather than
quietly never appearing in the gallery.

## Placement geometry

`place(source, screen, fit)` answers how a source image of a given pixel size
is drawn onto a screen of a given pixel size: a `Placement` carrying the
destination rectangle, the source rectangle sampled into it, and whether the
source repeats. Those three fields are jointly sufficient for every fit and
no fit can be expressed outside them, so a consumer cannot mis-draw a
placement:

- `Fill` — cover the screen, cropping the overflow, centred.
- `Fit` — contain the whole image, letterboxed, centred.
- `Stretch` — the exact screen size, ignoring aspect ratio.
- `Centre` — 1:1, centred, cropped when larger than the screen.
- `Tile` — 1:1, repeated from the origin.

The function is pure and total: all arithmetic is carried in `u64` and
clamped back into range, so every size up to `u32::MAX` and every extreme
aspect ratio is handled without a panic or a division by zero, and `None` is
returned **only** for a zero-extent source or screen.

`decode_request(source, screen, output, fit)` gives the size a decoder must
produce for the **whole** image so that no part of the composition is
enlarged. A decoder does not hand back a crop; it hands back the whole image
at some scale, so a caller must ask for the scale at which the rectangle the
placement *samples* still carries at least as many pixels as the rectangle it
*fills* — `nominal * destination / sampled`. That is the destination extent
for `Stretch`, more than it for `Fill` (whose crop discards part of the width
or height, so what remains must be denser), less than it for `Fit`'s
letterbox, and the nominal size itself for `Centre` and `Tile`, which draw
source pixels one-for-one and are only correct at that scale. Asking for
exactly this keeps a decode honest in both directions: asking for less leaves
the resampler enlarging pixels the file could have supplied, and asking for
more decodes detail nothing can show — for a 4K master bound for a gallery
tile, the difference between a one-eighth-scale decode and a half-scale one,
sixteen times the work for a picture the size of a postage stamp. Never more
than the source itself, since an 8.3-megapixel master is never
decoded larger than the screen can use. `Tile` is the one exception: it draws
every source pixel at 1:1 and so needs the native size.

## API shape

- `settings::{parse, render}` — the bounded, fail-closed, line-numbered parse
  and the canonical render.
- `DesktopSettings{wallpaper, fit, backdrop, icons, sort}` and its `Default`
  — the document model.
- `WallpaperChoice::{None, Image}`, `WallpaperPath::{new, as_str}`,
  `WallpaperPathError::{TooLong, Malformed}` — the validated wallpaper value.
- `WallpaperFit::{Fill, Fit, Stretch, Centre, Tile}`,
  `Backdrop::{Theme, Colour}`, `Rgb::{new, from_hex, to_hex}`,
  `IconFlow::{Leading, Trailing}`, `IconSort::{Name, Kind, Size, Date}`,
  `CursorSize::{Normal, Large, Larger, Largest, percent, side}` — the
  closed value vocabularies.
- `SettingsKey::{ALL, PINBOARD, APPEARANCE, name, from_name, value_of}` — the
  closed key registry and its two groups;
  `DesktopSettings::{load, document, document_of}` and `merge` — the two
  readings, the canonical render, and the per-group one a surface posts;
  `DocumentRefusal` — the strict reading's reasons.
- `catalog::{WALLPAPER_STORE, DEFAULT_WALLPAPER_CATEGORY, DEFAULT_WALLPAPER,
  category_path, wallpaper_path, default_wallpaper_path,
  is_wallpaper_category_name, is_wallpaper_file_name, catalog_categories,
  catalog_entries, CatalogEntry}` — the shipped set and the listing model.
- `fit::{place, decode_request, nominal_source_size, Placement}` — the
  placement geometry.
- `PINBOARD_PUBLISHER` — the desktop session's signed bundle identifier, the
  one spelling a reader hands to `tairix_appdata::read_published`.
- `ApplyOutcome` and (behind `rt`) `apply` — the one client every surface
  asks the session to adopt a document with.

The crate is `no_std` + `alloc`, forbids `unsafe`, performs no I/O, holds no
authority, is host-unit-tested beside the code, and is fuzzed by
`tests/fuzz_wallpaper_settings.rs`. Stability tier: experimental
(`lib/wallpaper/README.md`). The staged design is `plans/PINBOARD.md`.

## The pane the desktop hands over

`WALLPAPER_PANE` is the name the backdrop menu's *Change Background…* row
hands to the Settings application as its launch target. It lives here, with
the rest of the wallpaper vocabulary those two already share, because
neither may depend on the other: the session names it, and Settings resolves
it against its own closed pane registry. It confers nothing, so a name that
application does not recognise leaves its window where it was.
