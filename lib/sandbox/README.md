# tairix-sandbox

The parser-sandbox seam for TAIRiX (`lib/sandbox`).

Every parser of untrusted input runs in a minimum-capability sandbox
process (`docs/src/security/sandbox.md`). The kernel primitive — the
`SPAWN_FLAG_SANDBOX` spawn mode with its empty capability record and closed
syscall allow-list — makes such a process *exist*; this crate is the one
user-space seam that makes it *usable*: a typed request/reply path from a
calling program to a sandboxed worker, with crash containment, worker
replacement, and stable log events. Every program that sandboxes a parse
imports this seam; a second per-app copy is forbidden.

## What it provides

- **The protocol** (`proto`): a length-framed byte protocol over any
  bidirectional channel (`Channel`), bounded by `MAX_FRAME` — a fixed
  validation bound, not a growable capacity. Both sides fail closed on an
  oversize or truncated frame.
- **The worker loop** (`worker`): `serve` reads request frames, hands each
  payload to a `Service`, and writes the reply frame; a closed request
  stream ends the worker cleanly. A `Service` is total: a malformed request
  is a typed error *reply*, never a panic.
- **The host side** (`host`): `ParserSandbox` sends a request and receives
  the reply over a worker its `Launcher` started. Any worker failure —
  crash, protocol violation, oversize reply — is contained: the caller
  receives a typed `SandboxError`, the worker is disposed of and replaced,
  and the event is logged with a stable `EventId` (this crate owns the
  `6000..7000` range). A parser crash never takes down the calling program.
- **The decode service** (`decode`): the first consumers behind the seam —
  executable-container summaries through `tairix-binfmt` and per-window
  instruction disassembly through `tairix-disasm`, with a bounded,
  fail-closed reply vocabulary. The caller-side helpers decode every reply
  fail-closed: a compromised worker can lie about bytes, never break the
  caller.
- **The help-render service** (`helpdoc`): a foreign bundle's help document
  is parsed and rendered inside the worker (`tairix-help`), and the
  caller-side `render_help` re-parses the reply through the `tairix-vt`
  streaming parser, admitting only the closed render-op set (printable
  text, line feeds, the bold/underline SGR pairs) and re-encoding it
  canonically — a forbidden escape, colour, OSC string, or truncated
  trailing sequence refuses the whole reply. A document-parse error
  round-trips typed (`HelpError`, code for code). `man` is the consumer.
- **The icon-rasterisation service** (`imagerender`): an application
  bundle's icon — SVG or PNG bytes — is sniffed, decoded, and rasterised
  to the caller's requested square side inside the worker
  (`tairix-svg`/`tairix-image`/`tairix-icon`/`tairix-raster`), and the
  caller-side `rasterise_icon` validates the reply's echoed side and exact
  pixel length before trusting the returned straight-alpha RGBA8 buffer. A
  PNG source is fitted inside the square preserving its aspect ratio and
  scaled through the crate's one shared resampler (`tairix-raster`'s
  alpha-weighted box filter, never nearest-neighbour); an SVG source
  rasterises directly through the shared vector-icon path. Either a typed
  refusal or a sandbox failure simply means the desktop session falls
  back to its own built-in glyph.
- **The wallpaper-placement service** (also `imagerender`, the same
  worker): a desktop wallpaper — a shipped master or a file the user
  picked, whatever `tairix-image` can decode (PNG and JPEG today; the
  format list grows with `tairix-image` itself) — is sniffed, decoded, and
  placed onto the session's screen size across a three-op sequence:
  `OP_WALLPAPER_PREPARE` decodes the source at the smallest scale its
  format offers that still covers the destination (`tairix-image`'s
  `decode_fitted`, bounded by `MAX_WALLPAPER_DECODE_PIXELS`, so an
  8.3-megapixel master bound for a 1080p screen is decoded at a quarter of
  its pixels rather than in full) and computes its placement
  (`tairix-wallpaper`'s `place`), holding both in the worker;
  `OP_WALLPAPER_BAND` draws and returns a run of destination rows at a
  time, since a screenful of straight-alpha RGBA8 can exceed `MAX_FRAME`
  above 1080p and the frame bound is never raised to fit a larger reply;
  `OP_WALLPAPER_RELEASE` drops the held source. The destination is
  bounded by `MAX_WALLPAPER_WIDTH`/`MAX_WALLPAPER_HEIGHT` (4K) on both
  sides of the seam. A tiled fit repeats the decoded source at 1:1; every
  other fit resamples the placement's source rectangle into its destination
  rectangle through the same shared resampler the icon path uses, and any
  pixel the placement does not cover (a letterboxed fit, a source smaller
  than the screen) is left fully transparent so the desktop's own
  backdrop colour shows through — this service never draws a backdrop.
  The caller-side `render_wallpaper` drives the whole sequence, validates
  every band's echoed geometry and exact length fail-closed, assembles
  the final buffer, and always releases the held source afterwards — on
  the success path and on every error path alike. A prepare replaces any
  source (and placement) an earlier prepare left held on the same
  (reused) worker.
- **The production transport** (`rt`, feature `program`, bare-metal only):
  the parent launches **its own binary** in a worker role via
  `SpawnAttach::sandbox` with two pipes wired to the worker's fd 0/1, and
  the worker serves over its standard streams — exactly the surface the
  kernel sandbox allow-list admits.

## Security posture

- The sandbox worker is treated as hostile the moment it has parsed a
  byte: reply frames are bounded and every reply field is validated before
  the caller acts on it (fail closed).
- The seam adds no authority: the worker holds only the two pipe ends its
  parent wired at spawn; the kernel enforces the rest
  (`docs/src/security/sandbox.md`).
- Fuzzed: `fuzz_sandbox` (the decode, helpdoc, and imagerender service
  request decoders — icon and wallpaper alike — and the caller-side reply
  decoders/validators) is enrolled in `cargo xtask fuzz`.

## Design

- `no_std` + `alloc`; `unsafe` only in the `program`-feature transport's
  syscall marshalling (none in the protocol/seam core).
- Host-testable end to end: the `Launcher`/`Channel` seams take in-process
  fakes exactly as the `Fs`/`Tty` seams do elsewhere.

## Stability

Tier: `experimental`.
