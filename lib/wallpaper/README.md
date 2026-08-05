# tairix-wallpaper

Stability tier: **experimental**.

The desktop pinboard wallpaper engine: the per-user pinboard settings
document (wallpaper choice, fit, backdrop colour, icon flow, sort order),
the shipped default wallpaper catalog and its bounded fail-closed listing
model, and the one pure wallpaper-placement geometry the desktop renderer
and the chooser's preview both draw through. It defines the validated
settings model (`PinboardSettings`), the line grammar and its closed key
registry (`SettingsKey`), the bounded fail-closed parser, and the canonical
render — plus the shipped wallpaper identity (`WALLPAPER_STORE`,
`DEFAULT_WALLPAPER`) and the placement geometry (`place`, `decode_target`).

The settings document is data on the volume, never a compiled-in table: a
per-user store at `/Users/<u>/Settings/Pinboard/pinboard.conf`
([`user_settings_path`]). The pinboard session that draws the desktop and
the chooser (`wallpaper.app`) that edits the settings both go through this
engine, so a writer and a reader can never disagree about what the pinboard
says. An absent store means "the documented defaults", not an error.
Pinboard settings are per-user state only; there is no machine-wide store.

The grammar is one `key value` line per setting, `#` beginning a comment to
end of line, blank and comment-only lines carrying no setting, and every
value drawn from its key's own closed vocabulary:

| Key         | Value                                              | Default                                       |
|-------------|----------------------------------------------------|-----------------------------------------------|
| `wallpaper` | `none`, or an absolute path to an image            | `/System/Graphics/Wallpapers/tairix-dark.jpg` |
| `fit`       | `fill` \| `fit` \| `stretch` \| `centre` \| `tile` | `fill`                                        |
| `backdrop`  | `theme`, or six bare hex digits `rrggbb`           | `theme`                                       |
| `icons`     | `leading` \| `trailing`                            | `leading`                                     |
| `sort`      | `name` \| `kind` \| `size` \| `date`               | `name`                                        |

A colour is written **bare** — `112233`, never `#112233` — because the
grammar's own comment marker cuts a line at the first `#`, which would
truncate a `#`-prefixed colour away before any colour parser saw it. The
crate therefore has exactly one spelling of a colour: [`Rgb::from_hex`]
reads bare digits and [`Rgb::to_hex`] writes them, so a consumer cannot pick
a spelling the document cannot hold. `render` emits every key, in registry
order, including one still at its default, so a render/parse round trip is
exact.

A settings document is untrusted input: the parser is bounded
(`MAX_SETTINGS_LEN`, `MAX_WALLPAPER_PATH_LEN`) and refuses the **whole**
document ([`SettingsError`], with the offending line) on anything it does
not fully understand — an unknown key, a duplicate key, a missing or
malformed value, or an over-long document. A reader that cannot fully
parse a store runs on [`PinboardSettings::default`] rather than guessing at
a partial intent, and a writer refuses the edit outright.

The five shipped wallpaper masters ship read-only at `WALLPAPER_STORE`
(`/System/Graphics/Wallpapers`), discovered at build time from
`lib/wallpaper/assets/` by `tools/syshelp` — never a hand-maintained list.
Each master is authored no larger than `lib/sandbox`'s
`MAX_WALLPAPER_WIDTH`×`MAX_WALLPAPER_HEIGHT` (3840×2160): JPEG entropy
decoding cannot skip blocks, so a source pixel beyond what the renderer
will ever draw costs decode time no screen can use. `catalog_entries` is
the one bounded, fail-closed definition of which files in a directory
listing a chooser may offer: it performs no I/O of its own, filtering to
the decodable extensions, rejecting illegal names, skipping oversized
files, and capping and sorting the result.

The fit geometry (`place`, `decode_target`) is pure arithmetic with no
rendering of its own: given a source image size, a screen size, and a
`WallpaperFit`, it answers the destination rectangle, the sampled source
rectangle, and whether the source tiles — every dimension checked/widened
through `u64` so it never panics and never divides by zero, however extreme
the aspect ratio.

The crate performs no I/O and holds no authority: reading and writing the
document, and listing a wallpaper directory, go through the secured VFS
under the caller's own kernel-attested identity — a per-user store is an
ordinary write under that user's own identity. A wallpaper path surviving
this crate's validation still names untrusted image content; decoding it
happens only inside the parser sandbox (`lib/sandbox`), never here.

`no_std` + `alloc`; host-unit-tested beside the code and fuzzed by
`tests/fuzz_wallpaper_settings.rs`. The staged design is
`plans/PINBOARD.md`; the subsystem page is `docs/src/lib/wallpaper.md`.
