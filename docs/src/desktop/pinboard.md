# The desktop pinboard

The **pinboard** is the desktop backdrop: the wallpaper drawn behind
everything, the icons for the logged-in user's `Desktop` folder drawn over
it, and the context menu that appears when the backdrop is right-clicked.
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
settings, or a re-list that moved the icons. See
[the session's desktop layer](./session.md#the-desktop-layer-wallpaper-or-backdrop-then-icons).

## The pieces

| where | what it owns |
|---|---|
| [`lib/wallpaper`](../lib/wallpaper.md) | the settings document, the shipped wallpaper catalog, and the placement geometry |
| [`lib/image`](../lib/image.md) | decoding a wallpaper (PNG and JPEG), including reduced-scale decode |
| `lib/raster` | the one image resampler both the icon and wallpaper paths use |
| [`lib/sandbox`](../security/sandbox.md) | decoding and placing a wallpaper inside a capability-empty worker |
| `lib/browse` | the icon grid, its two arrangements, the shared sort, and the new-folder naming rule |
| `userland/gui/session` | the pinboard itself: the layer, the menu, the settings, and the apply service |
| `userland/apps/wallpaper` | the chooser the user actually clicks |

Nothing about the pinboard lives in the kernel or in a driver.

## The settings document

One small text document per user, at
`<home>/Settings/Pinboard/pinboard.conf`, in the same bounded, fail-closed
`key value` grammar the [taskbar's pins](../lib/taskpins.md) use. It carries
five keys: which wallpaper, how it is fitted, the backdrop colour behind it,
which corner the icons arrange from, and how they are sorted.
[The `lib/wallpaper` page](../lib/wallpaper.md) is the reference for the
grammar, the defaults, and the bounds.

Two properties matter more than the format:

- **Absent is not broken.** A fresh account simply has no document, and the
  defaults apply silently. An *unusable* one (unreadable, oversized,
  non-UTF-8, malformed) yields the defaults **plus** a warning on `stderr`
  — the desktop comes up and says why, rather than guessing at a
  half-parsed intent or refusing to start.
- **The session is the only writer.** Nothing else rewrites the document,
  and the in-memory settings adopt an edit only *after* the write to disk
  succeeded, so what is on screen and what is on disk cannot diverge.

## Changing the settings

The chooser app and the backdrop menu both **ask**; the session decides,
applies, and persists. The rendezvous is `PINBOARD_ENDPOINT`, a reserved,
seat-scoped call endpoint in `lib/abi`, bound like the notification and
window rendezvous: the session that owns the seat serves the pinboard shown
on it, and nothing else may.

The request carries the **rendered settings document** rather than a struct
of discriminants. That is deliberate: a second encoding of the same model
beside the document's own grammar would be two definitions of one thing,
and the two would eventually disagree.

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

Reading is not brokered at all: the chooser reads the user's own document
directly, because a reader needs no coordination.

## Drawing the wallpaper

A wallpaper is untrusted input — a shipped master no less than a file the
user picked — so it is never decoded in the session's address space. The
session reads the bytes under its own identity, bounded, and hands them to
the [parser sandbox](../security/sandbox.md), which decodes the image,
places it, and returns the finished pixels.

Three properties make this affordable:

- **Reduced-scale decode.** The shipped masters are 8.3-megapixel JPEGs. The
  decoder picks the smallest DCT scale that still covers the screen, so a
  1920×1080 desktop never materialises 8.3 million pixels to throw most of
  them away.
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

### Fits

| fit | what it does |
|---|---|
| `fill` | covers the screen, cropping the overflow, centred (the default) |
| `fit` | contains the whole image, letterboxed against the backdrop, centred |
| `stretch` | exactly the screen, ignoring the aspect ratio |
| `centre` | 1:1 in the middle, cropped if it is larger than the screen |
| `tile` | 1:1, repeated from the origin |

The geometry is one pure function in `lib/wallpaper`, shared by the desktop
and by the chooser's preview, so a preview can never disagree with the
desktop about what a fit will do.

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

## The context menu

A right-click on the backdrop opens the pinboard menu at the pointer, built
from the shared [menu control](./widgets.md) and presented as a popup window
the same way the taskbar's menus are. It offers: `Open` (only when the click
landed on an icon), `New Folder`, the four sort orders, the two
arrangements, `Refresh`, `Open Desktop Folder`, and `Change Background…`,
with the active sort and arrangement marked.

Managing an entry — rename, copy, delete, properties — is deliberately
absent. Those verbs belong to [the file manager](./apps.md), which owns them
whole, and `Open Desktop Folder` is one row away; a half-implemented second
copy of them here would be duplication.

The menu never acts on its own authority. It names a command; the session
carries it out, and reports on `stderr` anything it could not do — a
refused folder creation, a chooser that would not launch — leaving the
desktop unchanged rather than failing silently or dying over a refusal.

## The chooser

`wallpaper.app` is an ordinary graphical application bundle: it is launched
from the menu, typeable by name, and holds no special authority. It offers
the fit, backdrop, arrangement, and sort, and applies by sending the
rendered document to the session. A refusal is reported in its own window;
it never fabricates success and never exits over one.

The window is a large preview beside the four settings, a scrolling gallery
of the shipped wallpapers beneath them, and the two actions in the footer:

```text
+--------------------------------------------------------------+
|  +---------------------------+  Fit      [ Fill screen   v ] |
|  |       live preview        |  Backdrop [ Theme         v ] |
|  |                           |  Icons    [ Top left      v ] |
|  +---------------------------+  Sort     [ Name          v ] |
|  Wallpapers                     tairix-dark.jpg              |
|  +---------------------------------------------------+ +--+  |
|  |  [tile]  [tile]  [tile]  [tile]  [tile]           | |##|  |
|  +---------------------------------------------------+ +--+  |
|  Applied.                                  [Close] [Apply]   |
+--------------------------------------------------------------+
```

It is **driven by the pointer**, with the keyboard as a complete secondary
path: click a tile to select it and the preview follows, click a setting to
open its list, wheel or drag the gallery, click Apply. Every interactive
part is a shared control from [the control set](./widgets.md) — the
drop-downs, the buttons, the scrollbar — held for the life of the window so
each owns its own hover, press and drag state, and the gallery is the
shared icon-grid engine the file manager and the desktop's own icon field
use. The chooser therefore defines no control and no grid of its own.

The preview and every tile are rendered through the same sandboxed path the
desktop uses, so the chooser decodes nothing itself. A tile is the
wallpaper at tile size, always placed to fill its square — it says *which*
wallpaper it is — and the preview panel is where the chosen fit is shown.

The preview is a **scale model of the real screen**. The chooser asks the
session for the seat's desktop before it opens its window, so it knows the
screen's exact extent; inside the preview panel it draws the largest box
with the screen's aspect ratio that fits, centred, and renders the
wallpaper into it as the desktop would. The sandboxed render is told the
screen it models as well as the surface it writes and scales the source
accordingly, so `centre` and `tile` — which are defined in screen pixels —
model correctly instead of drawing at 1:1. The extent is part of the
preview request, so a preview rendered for one screen can never be shown as
if it were for another, and a change of screen re-renders it. What the
preview shows is what the desktop will show (`plans/PINBOARD.md` §8).

## Headless

The pinboard is part of `userland/gui/*` and is therefore optional in
exactly the way the rest of the desktop is (`AGENTS.md` §17.3). A headless
image omits it, and nothing outside `userland/gui/*` depends on it. The
`lib/*` crates it rests on — the settings engine, the decoder, the
resampler, the sandbox — carry no GUI dependency and are useful without it.
