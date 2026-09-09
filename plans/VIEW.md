# VIEW — the picture and document viewer (`View.app`)

Binding under `AGENTS.md`. What `View.app` is, which formats it claims, where
each piece of the work lives, and the seams that keep the app itself free of
decoding and of I/O.

`viewer.app` — the read-only *text* viewer that first proved the app-window and
picker-delegation paths (`plans/APPWIN.md` AW5, `plans/CAPABILITY_USE.md` CU6) —
is **deleted**, not evolved. Its purpose was a proof; its design was a text
pager holding a `ScrollModel` over sanitised lines, which is not a viewer for
pictures. Text belongs to `edit.app`, which already renders it: two apps
claiming `text/plain` would be two text-rendering paths, and the charter permits
one.

## What it is

A first-class viewer for **pictures and documents**, on par with Preview: the
thing the file manager hands a picture to (`Activation::OpenFile` /
`OpenWith`, `plans/NEW-FILEMANAGER.md`), and a standalone app that asks the
session's trusted picker when launched with no document.

`instances = "multiple"`: several documents open side by side, because
comparing two pictures is the ordinary case.

**It is a viewer.** It holds no write capability and has no editing, saving,
export, annotation, or printing. That is not an omission to be filled in later;
it is what the app is, and it is why the app needs no filesystem authority of
its own.

### Formats, and what "supported" means

JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO, RISC OS Sprite, and PDF.

Every format the app claims is supported **completely** — every bit depth,
compression, colour space, and structural variant the format defines — not the
subset a common file happens to use. A format that can only be half-decoded is
not claimed at all. A file the decoder refuses fails closed to a **stated
reason drawn in the window and written to `stderr`**; never a blank surface,
and never a fabricated image.

Associations are **pictures and PDF only**. Text is `edit.app`'s.

## Where each piece lives, and why there

| Piece | Home |
|---|---|
| GIF, TIFF, WEBP, BMP, ICO, Sprite decoders | `lib/image`, private modules |
| Multi-frame / multi-page decode | `lib/image` sequence API |
| Exact 90° rotation and flip | `lib/raster::surface` |
| Viewport-targeted SVG rasterisation | `lib/svg`, `decode` evolved in place |
| Untrusted decode | `lib/sandbox::imagerender` |
| The app-side window shell | `lib/window::app` |
| The app | `userland/apps/view` |
| PDF | `lib/pdf`, behind the page source (next change) |

`lib/image` is already *the* raster registry: `ImageFormat`/`sniff`/`probe`/
`decode` dispatch, and `DecodeLimits`/`RasterImage`/`DecodeError` carry the
fail-closed discipline. Sibling crates would duplicate all of it, and the icon
and wallpaper pipelines gain every new format for free the moment it lands.

The rotation is a pixel permutation, so it belongs to the one rasterisation
path the charter allows; an app-local copy would be a second one.

Nothing is needed in the content-type registry — `lib/browse::media` already
carries every one of these types with an icon. Only the bundle's own
`associations` list is new.

## The page source — one seam, every document

The engine reaches every document through **one** seam, so `lib/pdf` drops in
behind it unchanged:

- page/frame count,
- per-page natural geometry and metadata,
- render one page at a target size.

Designed before PDF exists precisely so PDF needs no rework, and shaped so a
still picture is the one-page case rather than a special case.

### Sequences are stateful, because disposal makes them so

A GIF or animated WEBP frame is composited **onto its predecessors** under the
format's disposal model, so a per-index decode would be both wrong and O(n²).
The sequence decoder therefore holds the composition canvas and yields
composited frames in order: stepping is O(1) amortised. Page-addressed formats
(TIFF, ICO, PDF) expose the same shape with independent entries.

### Animation is one-shot and tickless

Playback waits with `waitset_wait(set, timeout_ns, …)` against a deadline from
`clock_get()`: the loop wakes on the next window event **or** the frame
deadline, whichever is first. No periodic tick, no spin, and no timer armed
while playback is paused.

## Decoding untrusted files

A viewer must never decode a file in its own address space, and the worker must
hold no filesystem reach at all.

`SpawnAttach` wires exactly the four standard streams, so there is no slot to
hand a worker a document descriptor — and handing it one would widen the
minimum-capability sandbox anyway. So:

1. `Run` holds the read-only descriptor it was given (inherited at spawn, or
   redeemed from the picker's one-shot grant).
2. It reads the bytes under a **fixed input-byte ceiling** — a containment
   bound, not a capacity, so it does not scale with the machine.
3. It streams them into the worker in `proto::MAX_FRAME`-bounded frames.
4. It drives probe and per-page render requests, collecting pixels through the
   band protocol `imagerender` already defines.

The parent trusts nothing about a reply beyond its length and echoed geometry.
`fs_read` is positional, so the ceiling bounds what is **resident**, not what is
addressable.

## The app

**Engine (`src/lib.rs`)** — host-tested, no window, no I/O:

- the document model over the page source;
- a viewport carrying continuous zoom, fit modes (fit, fit-width, actual size),
  clamped pan, and rotation/flip;
- one `Layout::for_window(w, h, theme, scale)` producing every `Rect` that
  render, hit-test, and the tests read, so the three cannot disagree;
- renderers that **paint only from state**;
- a decoded-surface cache under `lib/reclaim`'s budget and pressure bands;
- one pure `InputEvent` entry point returning state change plus damage;
- a request/answer desk `Run` services — **the engine performs no I/O**.

Composed from the existing `lib/controls`, painting no control of its own: a
`Toolbar` of `IconButton`s, two `ScrollBar`s, a `Slider` for zoom, `IconTile`s
in a `Panel` for the thumbnail sidebar, a `FactList` for the info panel, and
`damage::set` / `damage::move_mark` for every guarded write.

**`Run` (`src/run.rs`)** over the shared shell: the app-bar declaration, the
wait-set extended with the worker channel and the animation deadline, document
acquisition by `DOCUMENT_ROLE_ARG` + `STDIN` or `pick_file`, menus via
`open_menu` + `AppMenu` (session-owned plates — the app draws no menu pixel),
`set_tooltip` for the toolbar, and `Scrolled { dx, dy }` for the wheel.

Behaviour: zoom in/out/fit/actual size, drag pan with scrollbars when zoomed
in, rotate and flip, page/frame navigation with a thumbnail sidebar, animation
play/pause and frame step, an alpha checkerboard, ICO size selection, an info
panel (format, geometry, bit depth, colour space, frame/page count, file size,
and per-format specifics — GIF loop count, TIFF compression, Sprite mode), an
app-declared menu, a context menu, and keyboard equivalents throughout.

## `lib/window::app` — the shared app shell

The windowed-app boilerplate was copy-pasted across seven `Run` binaries: the
`WindowTransport` impl over `ipc_call(WINDOW_ENDPOINT, …)` was **byte-identical**
in all seven. `lib/window` already owns the app half of the channel, so the rest
belongs beside it rather than in an eighth copy.

Shared: the transport; the base park (event mailbox + memory-pressure band) an
app's own `EventSource` calls; `bind_event_mailbox`; `bring_up_desktop`;
`mode_for`/`region_bytes`; the four reserved exit codes and `fail`; and the
retained-`Surface` `present`/`resize` pair.

The **resize ordering is the load-bearing part**: allocate the spare surface,
create the new frame region, grant it, ask the server to resize, and only then
swap. A refusal at any step leaves the old geometry standing, so a refused
resize is an answer rather than a broken window.

Apps keep what genuinely differs — their own extra wait-set members and the
tokens for them. The shell reserves the base tokens so an app's own cannot
collide.

`userland/gui/session` and `userland/session/greeter` are **out of scope**: they
are the window *server* and the pre-session login surface, not app-side
clients, and their exit-code sets are their own.

## Status

- `plans/VIEW.md` and the jump-sheet row — **done**.
- `lib/window::app` shared shell, and the migration of all six other app-side
  consumers — **done**. `datetime`, `widgets`, `wallpaper`, and `switchboard`
  are single-window and took `AppWindow` whole; `datetime` additionally stopped
  allocating a window-sized surface per paint, because painting into the
  retained one is what the shell offers. `files` and `terminal` are
  multi-window and took the free functions only.
  - **Outstanding, and deliberately not forced:** the two multi-window apps
    still hold their own per-pane present/resize. `AppWindow` is single-window,
    so making them use it would have been the wrong shape rather than a
    de-duplication. The correct fix is to split a public `WindowPane` (id +
    frames + mode + surface, with `present`/`resize` taking the client) out of
    `AppWindow`, which both a single-window `AppWindow` and a multi-window
    app's `Vec<WindowPane>` then compose. That is the next piece of this step,
    not a permanent state.
- `lib/image` sequence API and the page-source-facing surface — planned.
- `lib/image` GIF, BMP/ICO, Sprite, TIFF, WEBP — planned.
- `lib/svg` viewport decode; `lib/raster` rotate/flip — planned.
- `lib/sandbox::imagerender` view operations — planned.
- `userland/apps/view` engine, `Run`, bundle, 13 Help locales — planned.
- Deletion of `userland/apps/viewer` and the reference sweep — planned.
- `lib/pdf` behind the page source — next change. Encrypted PDFs need MD5/RC4/
  AES, which `lib/crypto` deliberately does not carry; whether to admit those as
  interop-only primitives or refuse encrypted files fail-closed is a decision to
  take then.

## Verification

Per-format decoder tests over valid, malformed, truncated, and adversarial
input — every bit depth, compression, photometric, and disposal path,
degenerate geometry, overflow edges, and limits refused *before* allocation.
Structure-aware generators per format in the already-registered `fuzz_image`
harness. `lib/sandbox` loopback tests including a hostile worker reply (wrong
echoed geometry, wrong pixel length) being refused. Engine host tests for
viewport clamping, rotation/flip composition, layout at several `Scale`s, input
routing, cache eviction across pressure bands, animation deadlines, the
request/answer desk, and the three damage-correctness properties every app owes.
The re-pointed `filepick_qemu_aarch64` vertical, extended to open a real image
end to end from Files.
