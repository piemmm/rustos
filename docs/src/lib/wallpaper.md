# `tairix-wallpaper` — the desktop pinboard wallpaper engine

`lib/wallpaper` is the shared engine behind the desktop pinboard: the
per-user pinboard settings document, the shipped wallpaper set and the
listing model a chooser draws its thumbnail grid from, and the one wallpaper
placement geometry the desktop renderer and the chooser's preview both draw
through. The settings are **data on the volume**, never a compiled-in table:
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
  scope, so no other program the user launches — including the chooser — can
  write the desktop's document at all. The chooser asks over the pinboard
  channel and the session decides.
- **Any application may read it**, by naming `PINBOARD_PUBLISHER` on a
  request shape that carries no scope field, so "read the desktop's private
  settings" is not a request that exists. That is the sanctioned sharing
  channel replacing `/Users/<u>/Settings/Pinboard/pinboard.conf`, which every
  application of that user could also *rewrite*.

An **absent** store is not an error: it means the documented defaults
(`PinboardSettings::default`), and so does an account whose session has never
run. A document naming only some keys leaves the rest at their default.
Pinboard settings are per-user state only; there is no machine-wide store,
and the published scope has no layer beneath it, so nobody can make the
desktop appear to say something it never said.

## Two readings, deliberately different

`PinboardSettings::load` is the **tolerant** one, for a document held in a
store: a value the registry refuses leaves that one field at its documented
default and is *named* to the caller, so one stale setting costs only itself
and never blanks a user's desktop. It reads through `tairix_appconf::Lookup`,
so the same loader serves the session's own published-scope handle and the
`Document` a foreign read answers with.

`decode` is the **strict** one, for a document that arrived over the pinboard
channel: a line outside the grammar, a key outside the registry, or a value
outside a key's closed set is a defect in the *sender* rather than something
a person typed, and adopting a desktop the sender did not describe is worse
than refusing it. `DocumentRefusal` names which.

`PinboardSettings::document` renders the canonical form both readings accept:
every registry key, in registry order, so a render/read round trip is exact.
Publishing to the store instead goes key by key, so only what actually
changed is written.

## The registry

Every line is drawn from the closed `SettingsKey` set, and every value from
that key's own closed vocabulary:

| Key         | Value                                             | Default                                       |
|-------------|---------------------------------------------------|-----------------------------------------------|
| `wallpaper` | `none`, or an absolute path to an image           | `/System/Graphics/Wallpapers/tairix-dark.jpg` |
| `fit`       | `fill` \| `fit` \| `stretch` \| `centre` \| `tile`| `fill`                                        |
| `backdrop`  | `theme`, or six bare hex digits `rrggbb`          | `theme`                                       |
| `icons`     | `leading` \| `trailing`                           | `leading`                                     |
| `sort`      | `name` \| `kind` \| `size` \| `date`              | `name`                                        |

Keys and values are case-sensitive: each has one canonical spelling.

A colour is written as **bare** hex digits — `112233`, never `#112233`. The
document's own comment grammar cuts a line at the first `#`, so a
`#`-prefixed colour would be truncated away before any colour parser saw it.
There is therefore exactly one spelling of a colour in the crate:
`Rgb::from_hex` reads bare digits and `Rgb::to_hex` writes them, so a
consumer cannot pick a spelling the document cannot hold.

`render` always emits **every** key in `SettingsKey::ALL` order, including a
key still at its default, so the document a user opens always shows the whole
registry and `parse(render(s)) == s` exactly. Adding a key means adding a
`SettingsKey` variant, its `PinboardSettings` field, and its parse/render
arms in the same change; there is no free-form key namespace and no second
store.

## Security

A pinboard settings document is **untrusted input** to every consumer, and the
two readings above bound it the same way: the format engine bounds the
document, the line, the key and the value, and `MAX_WALLPAPER_PATH_LEN` bounds
the one value that carries a path. Neither reading ever half-applies a
document — `decode` refuses the whole thing, `load` leaves the refused field
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
compile-time assertion), `MAX_WALLPAPER_BYTES` (8 MiB per wallpaper file), and
`MAX_WALLPAPER_CATALOG_ENTRIES` (256 offered wallpapers).

The engine performs no I/O and holds no authority: the document is read and
written through the app-data service under the caller's own kernel-attested
identity, and listing a wallpaper directory goes through the secured VFS.

## The shipped set and the catalog

The OS ships its wallpaper masters read-only under `WALLPAPER_STORE`
(`/System/Graphics/Wallpapers`), discovered at build time from
`lib/wallpaper/assets/` by `tools/syshelp` and planted by the image builder —
never a hand-maintained list. `DEFAULT_WALLPAPER` names the default master
and `default_wallpaper_path()` spells its absolute path, which is also the
default `wallpaper` setting.

`catalog_entries` is the one definition of which files a chooser may offer.
It performs **no** I/O — the caller lists the directory and passes
`(name, byte length)` pairs — and admits an entry only when its name is a
legal plain file name (no path separator, no control character, not `.`/`..`),
its extension is one of `.jpg`, `.jpeg`, `.png` (case-insensitively), and its
size is at most `MAX_WALLPAPER_BYTES`. Anything else is silently dropped, so a
directory mixing wallpapers with unrelated files yields only the wallpapers
rather than a refusal of the whole listing. The result is sorted by name and
capped at `MAX_WALLPAPER_CATALOG_ENTRIES`.

`is_wallpaper_file_name` is that name contract on its own, so
`tools/syshelp`'s build-time discovery applies exactly the definition the
runtime applies: a shipped master the desktop could never offer, or one over
the byte bound, fails the **build** rather than quietly never appearing in the
chooser.

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
- `PinboardSettings{wallpaper, fit, backdrop, icons, sort}` and its `Default`
  — the document model.
- `WallpaperChoice::{None, Image}`, `WallpaperPath::{new, as_str}`,
  `WallpaperPathError::{TooLong, Malformed}` — the validated wallpaper value.
- `WallpaperFit::{Fill, Fit, Stretch, Centre, Tile}`,
  `Backdrop::{Theme, Colour}`, `Rgb::{new, from_hex, to_hex}`,
  `IconFlow::{Leading, Trailing}`, `IconSort::{Name, Kind, Size, Date}` — the
  closed value vocabularies.
- `SettingsKey::{ALL, name, from_name, value_of}` — the closed key registry;
  `PinboardSettings::{load, document}` and `decode` — the two readings and
  the canonical render; `DocumentRefusal` — the strict reading's reasons.
- `catalog::{WALLPAPER_STORE, DEFAULT_WALLPAPER, default_wallpaper_path,
  is_wallpaper_file_name, catalog_entries, CatalogEntry}` — the shipped set
  and the listing model.
- `fit::{place, decode_request, nominal_source_size, Placement}` — the
  placement geometry.
- `PINBOARD_PUBLISHER` — the desktop session's signed bundle identifier, the
  one spelling a reader hands to `tairix_appdata::read_published`.

The crate is `no_std` + `alloc`, forbids `unsafe`, performs no I/O, holds no
authority, is host-unit-tested beside the code, and is fuzzed by
`tests/fuzz_wallpaper_settings.rs`. Stability tier: experimental
(`lib/wallpaper/README.md`). The staged design is `plans/PINBOARD.md`.
