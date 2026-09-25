# The desktop pinboard

The **pinboard** is the desktop backdrop: the wallpaper drawn behind
everything, the icons for the logged-in user's `Desktop` folder drawn over
it, and the menu that appears when the backdrop is right-clicked.
The binding design is `plans/PINBOARD.md`; this page is the reference for
how it is put together and where each decision lives.

The pinboard is a *layer*, not a window. The compositor
(`AGENTS.md` §10, [the window manager](./wm.md)) keeps one desktop layer
beneath every window, and the session paints the pinboard into it. A
wallpapered desktop therefore costs the compositor nothing extra to
*composite*: it blends the same single surface it always did, whatever the
picture.

What it does cost is *repainting* that layer, and only there. The desktop is
the bottom of the stack, so marking all of it recomposites every window above
it and throws away every frosted backdrop over it — on a 1080p screen, most of
a megapixel of blur to move one highlight. So the model reports the icon cells
a gesture actually changed and the session repaints only those
(`DesktopShell::present_desktop_area`); the whole layer is repainted only when
the whole layer changed — bring-up, a new wallpaper, a theme switch, adopted
settings, or a re-list that moved the icons. Icon artwork arriving from the
decode desk is not one of those: a landed decode can only change the picture
inside a tile, so `Desktop::mark_icons` reports the shown cells and a column
with nothing in it costs no frame at all. See
[the session's desktop layer](./session.md#the-desktop-layer-wallpaper-or-backdrop-then-icons).

## The pieces

| where | what it owns |
|---|---|
| [`lib/wallpaper`](../lib/wallpaper.md) | the settings document, the shipped wallpaper catalog, and the placement geometry |
| [`lib/image`](../lib/image.md) | decoding a wallpaper (PNG and JPEG), including reduced-scale decode |
| `lib/raster` | the one image resampler both the icon and wallpaper paths use |
| [`lib/sandbox`](../security/sandbox.md) | decoding and placing a wallpaper inside a capability-empty worker |
| `lib/browse` | the icon grid, its two arrangements, the shared sort, and the new-folder naming rule |
| `userland/gui/session` | the pinboard itself: the layer, the backdrop menu's row model, the settings, and the apply service |
| [`userland/apps/settings`](settings.md) | the Wallpaper pane the user actually clicks, over the session's two served requests |

Nothing about the pinboard lives in the kernel or in a driver.

## The settings document

One small document per user, in the desktop session's **published** app-data
scope ([the app-data client](../lib/appdata.md), `plans/APPDATA.md` §3.11).
It carries five keys: which wallpaper, how it is fitted, the backdrop colour
behind it, which corner the icons arrange from, and how they are sorted.
[The `lib/wallpaper` page](../lib/wallpaper.md) is the reference for the
registry, the defaults, and the bounds.

Three properties matter more than the format:

- **Absent is not broken.** A fresh account has published nothing, and the
  defaults apply silently. A value this build's registry does not accept
  leaves *that one setting* at its default **plus** a warning on `stderr` —
  the desktop comes up and says why, rather than guessing at a half-parsed
  intent or refusing to start.
- **The session is the only writer, by construction.** An application
  publishes only its *own* scope, so no other program the user launches —
  including Settings — can write the desktop's document at all. The
  in-memory settings adopt an edit only *after* the publish succeeded, so
  what is on screen and what is stored cannot diverge.
- **Any application may read it**, by naming the session's bundle identifier
  on a request shape that carries no scope field — so Settings can show
  what is in effect without being able to reach anything else the session
  keeps. That replaces the hand-rolled `~/Settings/Pinboard/pinboard.conf`
  a settings surface used to open directly, a file every application of
  that user could also rewrite.

## Changing the settings

The backdrop menu and the Settings application both **ask**; the session
decides, applies, and persists. The rendezvous is
`PINBOARD_ENDPOINT`, a reserved, seat-scoped call endpoint in `lib/abi`,
bound like the notification and window rendezvous: the session that owns the
seat serves the pinboard shown on it, and nothing else may.

The request carries the **rendered settings document** rather than a struct
of discriminants. That is deliberate: a second encoding of the same model
beside the document's own grammar would be two definitions of one thing,
and the two would eventually disagree.

The session **merges** the request over what it currently holds rather than
replacing it. More than one surface asks the desktop to change and none of
them shows every setting: the backdrop menu and Settings' Wallpaper pane
edit the pinboard keys, its Appearance and Accessibility panes edit the
appearance keys. Each renders only the keys it edits, and a key a sender did
not name keeps the value the desktop has — so choosing a picture cannot
reimpose whatever appearance another pane happened to open on, and vice
versa.
Taking the absent keys as their *defaults* would do exactly that, on every
single apply. A document the registry refuses is refused whole: the merge
runs on a copy, so a refusal partway through leaves the desktop untouched.

The security posture is worth stating plainly, because it is easy to get
wrong:

- The session serves a request only from a caller whose kernel-attested
  origin carries the session's own uid. Anything else is refused and
  logged.
- The document is configuration data and **carries no authority**. It
  *names* a wallpaper path; the session then reads that path under its own
  identity. A caller therefore cannot use the pinboard to reach a file it
  could not read itself — the classic confused-deputy shape, closed by
  construction.

Reading is not brokered at all: a surface reads the user's own published
document directly, because a reader needs no coordination.

## Drawing the wallpaper

A wallpaper is untrusted input — a shipped master no less than a file the
user picked — so it is never decoded in the session's address space. The
session reads the bytes under its own identity, bounded, and hands them to
the [parser sandbox](../security/sandbox.md), which decodes the image,
places it, and returns the finished pixels.

What makes this affordable:

- **Reduced-scale decode.** A JPEG decodes at the smallest DCT scale that
  still covers the screen, so a 1920×1080 desktop never materialises a 4K
  master's 8.3 million pixels to throw most of them away. PNG has no reduced
  scale and decodes whole.
- **Banding.** A screenful of RGBA exceeds the sandbox's fixed 8 MiB frame
  bound above 1080p. The bound is a defence and is not raised; the pixels
  are transported in bands instead.
- **Prepared once.** The session holds one prepared, screen-sized surface
  and re-prepares it only when the wallpaper, the fit, or the screen
  geometry changes. Nothing decodes, resamples, or parses on a frame path.
  The surface is held in the shared reclaimable-memory model, so a machine
  under pressure drops it and re-prepares on demand.
- **Prepared elsewhere.** The read and the sandbox round trip run on a worker
  thread that owns its **own** capability-empty sandbox worker, so the desktop
  comes up without waiting for a picture and a settings change does not freeze
  it. The icon rasteriser keeps the serve loop's own sandbox handle, untouched.
  The desktop keeps painting whatever it has until the new surface arrives; a
  picture prepared for a screen size or a choice the desktop has since left is
  discarded rather than stretched onto the wrong screen. See
  [the session](session.md).

A wallpaper that will not decode is not fatal: the desktop falls back to
the backdrop colour, reports why on `stderr`, and remembers the refusal, so
a bad file costs one attempt rather than one per frame.

A gallery that crawls on real storage is diagnosed by taking the file read
and the sandboxed render apart rather than by guessing: the two halves have
unrelated causes, and the decode is already a known quantity (a shipped
master decodes in about 15 ms at thumbnail scale and ~90 ms full-screen), so
a placement costing seconds is never the decoder.

The prepared picture is never cut to. Because it arrives whenever the worker
finishes — a second or so into the session at login, or mid-session when the
choice changes — it dissolves into whatever ground is on screen over the
theme's own `BackdropChange` span: over the backdrop colour at login, and over
the picture it replaces when the user picks another. See
[the session's backdrop crossfade](session.md#the-backdrop-dissolves-it-is-never-cut-to).

### Fits

| fit | what it does |
|---|---|
| `fill` | covers the screen, cropping the overflow, centred (the default) |
| `fit` | contains the whole image, letterboxed against the backdrop, centred |
| `stretch` | exactly the screen, ignoring the aspect ratio |
| `centre` | 1:1 in the middle, cropped if it is larger than the screen |
| `tile` | 1:1, repeated from the origin |

The geometry is one pure function in `lib/wallpaper`, shared by the desktop
and by every preview, so a preview can never disagree with the desktop about
what a fit will do.

## The icons

The pinboard lists the user's `Desktop` folder through exactly the same
machinery the file manager uses: the same directory seam, the same shared
sort, the same content-type classifier, the same grid, and the same
double-click activation ([desktop icons](./icons.md),
`plans/NEW-TASKBAR.md` T7/T16). Two things are settings:

- **Arrangement** — icons fill a column downward and grow a new column
  across, starting either from the leading edge (the Windows/KDE
  arrangement, and the default) or from the trailing edge. The two are
  exact mirror images of one another in the shared grid engine, not two
  layouts.
- **Sort** — by name, kind, size, or date, drawn from the shared sort, so
  the desktop and the file manager agree on what each ordering means.

A desktop icon is often a **shortcut** — a symbolic link the program
library's row menu asked the session to make (`plans/SYMLINKS.md` S5, see
[the session](./session.md#desktop-shortcuts)). It is classified and
activated as what it *names*: bundle-ness reads off the target's own leaf, a
folder or file is opened through the link, and one whose target has gone is
refused with its reason rather than launched blind.

## The backdrop menu

A right-click on the backdrop opens the desktop's own menu at the pointer. It
is a chain like every other menu on the system — the pinboard hands a row model
to the one [menu service](./menus.md) and keeps no shell of its own — and it
offers: `Open` (only when the click landed on an icon), `New Folder`, the four
sort orders, the two arrangements, `Refresh`, `Open Desktop Folder`, and
`Change Background…`, with the sort and the arrangement in force shown as their
group's chosen member and not offered again.

Managing an entry — rename, copy, delete, properties — is deliberately
absent. Those verbs belong to [the file manager](./apps.md), which owns them
whole, and `Open Desktop Folder` is one row away; a half-implemented second
copy of them here would be duplication.

The menu never acts on its own authority. It names a command; the session
carries it out, and reports on `stderr` anything it could not do — a
refused folder creation, a Settings window that would not launch — leaving
the
desktop unchanged rather than failing silently or dying over a refusal. Where
each row's command is resolved, and how its one answer reaches the session,
is [the session's own page](./session.md#the-backdrop-menu).

## Choosing a picture

The desktop picture is a **section of Settings**, not an application beside
it: the menu's `Change Background…` opens
[the Settings application](./settings.md) at its Wallpaper pane, where the
fit, backdrop, arrangement and sort are four form rows above a gallery of
the shipped pictures. A Settings already running is handed the pane and
navigates to it; a fresh one is given the same pane and opens on it.

**Settings holds no authority over any of it, and gains none for the
gallery.** Listing the shipped store needs a filesystem capability and
decoding a picture needs a parser sandbox, and Settings requests neither —
an application that will later carry Networking, Users and Storage must not
also hold the reach to read arbitrary files. So the gallery is *served*: the
session lists the read-only store once at its own bring-up and answers a
catalog page on request, and renders one candidate at a time into a
shared-memory region Settings created and granted. There is one sandboxed
decode path on the desktop instead of two, and no picture is ever decoded in
the address space of the application that browses them.

A render names a **catalog position**, never a path, so the request cannot
make the session read a file the caller chose. A picture in effect that the
catalog does not hold — one set before it was removed from the store — is
still offered and still selectable; it simply has no position to be rendered
at, so its tile draws its built-in glyph and its name.

The desktop renders one preview at a time, which is what bounds how much
decoding a browsing application can set going, and its own backdrop is
always prepared first: the picture the user is looking at never waits behind
a thumbnail. Every tile is requested and never awaited — a paint draws what
has come back and a placeholder for what has not — so the pane is usable
from its first frame and fills in as the answers land.

**An apply does not block the window.** The session answers only once its own
publisher has written the store, so the choice is rendered into the pinboard
half of the settings document (in memory, and refusable on the spot) and the
round trip is handed to a worker. The rows and the gallery stay live
throughout, and the answer is what becomes durable: a refusal puts the
selection back rather than leaving a picture on screen the next login would
not restore.

## Headless

The pinboard is part of `userland/gui/*` and is therefore optional in
exactly the way the rest of the desktop is (`AGENTS.md` §17.3). A headless
image omits it, and nothing outside `userland/gui/*` depends on it. The
`lib/*` crates it rests on — the settings engine, the decoder, the
resampler, the sandbox — carry no GUI dependency and are useful without it.
