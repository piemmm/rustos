# `tairix-wallpaper` — the desktop pinboard wallpaper engine

`lib/wallpaper` is the shared engine behind the desktop pinboard: the
per-user pinboard settings document, the shipped wallpaper set and the
listing model a chooser draws its thumbnail grid from, and the one wallpaper
placement geometry the desktop renderer and the chooser's preview both draw
through. The settings are **data on the volume**, never a compiled-in table:
a per-user store at `tairix_wallpaper::user_settings_path(home)`
(`/Users/<u>/Settings/Pinboard/pinboard.conf`). Because there is exactly one
definition of the document, of the catalog, and of the geometry, no two
consumers can disagree about what the settings say, which wallpapers exist,
or how a fit places one.

## The store

One text document per user: `key value` lines, one per setting; `#` begins a
comment to end of line; blank and comment-only lines carry no setting. Key
and value are split at the first whitespace run and the value is trimmed.

An **absent** store is not an error: it means the documented defaults
(`PinboardSettings::default`). A document naming only some keys leaves the
rest at their default. Pinboard settings are per-user state only; there is no
machine-wide store.

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

A pinboard settings document is **untrusted input** to every consumer. The
parser is bounded (`MAX_SETTINGS_LEN`, `MAX_WALLPAPER_PATH_LEN`) and refuses
the **whole** document (`SettingsError`, carrying the offending 1-based line
where one is meaningful) on anything it does not fully understand: an unknown
key, a duplicate key, a missing value, a value outside its key's closed set,
or an over-long document. A half-applied document would leave a desktop in a
state no user asked for, so a reader that cannot fully parse a store runs on
`PinboardSettings::default` rather than guessing at a partial intent, and a
writer refuses the edit outright.

A `wallpaper` value is validated as a canonical absolute session-view path
(`WallpaperPath`): an empty, relative, alias- or volume-id-rooted, embedded-
control-character, `#`-bearing, or over-long path is refused, never "fixed
up". Surviving validation still means the path names untrusted **content** —
the session reads it under its own identity and the image decoder sniffs and
bounds it in its own sandbox before a pixel is drawn. This crate decodes
nothing.

The bounds above are fixed validation limits on untrusted input, not growable
capacities: `MAX_SETTINGS_LEN` (document bytes), `MAX_WALLPAPER_PATH_LEN`
(1 KiB path), `MAX_WALLPAPER_BYTES` (8 MiB per wallpaper file), and
`MAX_WALLPAPER_CATALOG_ENTRIES` (256 offered wallpapers).

The engine performs no I/O and holds no authority: reading and writing the
document, and listing a wallpaper directory, go through the secured VFS under
the caller's own kernel-attested identity — a per-user store is an ordinary
write into the user's home.

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

`decode_target(source, screen, fit)` gives the source pixel box a decoder need
only produce for that placement — never more than the placement crops in, and
never more than the destination can show — so a 25-megapixel master is never
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
- `SettingsKey::{ALL, name, from_name}` — the closed key registry;
  `SettingsError::{line, kind}` and `ParseError` — refusal reasons.
- `catalog::{WALLPAPER_STORE, DEFAULT_WALLPAPER, default_wallpaper_path,
  is_wallpaper_file_name, catalog_entries, CatalogEntry}` — the shipped set
  and the listing model.
- `fit::{place, decode_target, Placement}` — the placement geometry.
- `PINBOARD_SETTINGS_SUBDIR` / `PINBOARD_FILE` / `user_settings_path` — the
  path spellings, defined once here.

The crate is `no_std` + `alloc`, forbids `unsafe`, performs no I/O, holds no
authority, is host-unit-tested beside the code, and is fuzzed by
`tests/fuzz_wallpaper_settings.rs`. Stability tier: experimental
(`lib/wallpaper/README.md`). The staged design is `plans/PINBOARD.md`.
