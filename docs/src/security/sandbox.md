# The parser sandbox: minimum-capability worker processes

`AGENTS.md` §19.5 requires every parser of untrusted input to run in a
minimum-capability sandbox process. This page documents the kernel
primitive that makes such a process exist: the **sandbox spawn mode**,
requested by a flag in the spawn attach block and enforced entirely
kernel-side. The user-space seam that hands bytes to a sandboxed parser
and receives the typed result (with crash containment and worker
replacement) builds on this primitive and is staged in
`.junie/fstree-next-plan.md` S8b.

## Requesting a sandbox

A spawn becomes a sandbox spawn by setting `SPAWN_FLAG_SANDBOX` in the
`SpawnAttach` block's `flags` word (`lib/abi/src/process.rs`; C callers
use `TAIRIX_SPAWN_FLAG_SANDBOX`). The flag can only ever *narrow* the child,
so requesting it needs no capability.

A sandbox block is canonical only when nothing ambient can flow into the
child, and `SpawnAttach::parse` refuses any other shape fail-closed —
one definition shared by the kernel's staging path and every userland
encoder:

- **Every fd wire is explicit.** Each of the four standard-descriptor
  wires must be `Closed` or `Handle` — never `Inherit` or `InheritSlot`.
  The only channels a sandbox holds are the descriptors its parent
  deliberately handed over (typically a pipe pair).
- **No credential switch.** `target_uid` must be `SPAWN_UID_INHERIT`.
- **No console.** The console selector must be `CONSOLE_INHERIT`; a
  console index would attach console-backed streams, which a sandbox
  never receives.
- **No reserved flag bits.** Any undefined `flags` bit refuses the block.

## What the kernel enforces

Three layers, each fail-closed, each with its own tests:

1. **Empty capability sets, structurally** (`kernel/sec`). The spawn
   admit path brands the child's `TaskCapabilities` with
   `as_sandboxed()`, which discards the user grant, the manifest
   request, and the effective set — whatever the program's manifest
   asked for. Because all three sets are dropped, no later re-derivation
   can resurrect a capability. `delegate` and `apply_token` refuse a
   sandboxed target outright (`PermissionDenied`, audited as a widening
   attempt) before looking at the payload, so not even a validly signed
   token can land capabilities on a sandbox.
2. **A closed syscall allow-list** (`kernel/syscall`). The dispatcher
   refuses every syscall from a sandboxed task except
   `sandbox_allows`'s list, *before* the per-syscall capability check
   and before any handler runs:

   `yield`, `exit`, `stream_read`, `stream_write`, `fs_read`,
   `fs_write`, `fs_close`, `mem_map`, `mem_unmap`

   These are exactly the self-scoped and descriptor-scoped operations a
   worker needs: run, block on and talk over the wired descriptors, and
   manage its own heap. Everything that names an object outside the
   task — a path (`fs_open`), an IPC endpoint, a resource reference, a
   process (`spawn`/`signal`/`wait`), a device, system state — is
   refused, so a compromised parser cannot even probe those surfaces.
   Each denial is audited with the stable `SyscallPermissionDenied`
   event. Widening the list is a security decision held to the
   capability-minimalism bar, and the exact list is frozen by a unit
   test.
3. **Descriptor-scoped I/O only.** `fs_read`/`fs_write`/`fs_close` and
   `stream_read`/`stream_write` operate on the caller's own descriptor
   table — authority the parent established at spawn — and a
   console-backed stream additionally requires `CAP_CONSOLE_READ`/
   `CAP_CONSOLE_WRITE` in-handler, which a sandbox (empty set) can never
   hold. With canonical wires a sandbox has no console-backed stream in
   the first place.

The parent keeps full lifecycle authority over its child: `wait` reaps
it, `signal` can kill it, and a crashed worker is observed exactly like
any other abnormal child exit. Nothing about the sandbox brand weakens
the parent's side.

## The user-space seam: `lib/sandbox`

The kernel primitive makes a sandboxed process *exist*; `lib/sandbox`
(`tairix-sandbox`) is the one user-space seam that makes it *usable*, so
the containment discipline is written once and every program that
sandboxes a parse imports it:

- **Protocol** (`proto`): a length-framed byte protocol over any
  bidirectional `Channel` (pipes in production, in-memory fakes in host
  tests), bounded by `MAX_FRAME`. Both sides refuse an oversize declared
  length before reading or allocating a payload byte.
- **Worker** (`worker`): `serve` reads a request frame, hands the payload
  to a total `Service`, writes the reply frame, and ends cleanly when the
  parent closes the request stream. A malformed request is a typed error
  *reply*, never a panic.
- **Host side** (`host`): `ParserSandbox` sends one request and blocks
  for the reply. Every worker failure — crash, protocol violation,
  oversize reply, exit without answering — is contained identically: the
  caller receives a typed `SandboxError`, the dead worker is reaped and
  **replaced**, and the event is logged with a stable id
  (`EventId(6000)` worker crashed, `EventId(6001)` worker unavailable;
  the crate owns `6000..7000`). A parser crash never takes down the
  calling program.
- **Production transport** (`rt`, feature `program`, freestanding only):
  the parent spawns **its own binary** in a worker role — two fresh
  pipes wired to the child's fd 0/1 through `SpawnAttach::sandbox`, the
  `--parser-sandbox-worker` argv marker, a blocking reap on disposal.
  The worker serves over its standard streams, exactly the surface the
  allow-list admits. "Its own binary" is named by the reserved
  `SPAWN_SELF` (`@self`) path token, never by `argv[0]` (data the
  spawner chose, not a spawnable spelling): the kernel substitutes the
  exact path it admitted the *caller* from — the `spawn_path` attested
  on its capability record — and runs the ordinary resolution and load
  gate over it. The token serves any spawn of the caller's own binary
  (sandboxed or plain — `plans/STRESSTEST.md`'s worker re-entry is the
  plain consumer) and only when the caller carries a spawnable path; a
  caller without one fails closed `NotFound`.
- **First consumers** (`decode`): executable-container summaries
  (`lib/binfmt`) and per-window instruction disassembly (`lib/disasm`)
  run inside the worker; the client-side helpers validate every reply
  field fail-closed, because a worker that has parsed hostile bytes is
  itself treated as hostile.
- **Help rendering** (`helpdoc`): a foreign bundle's help document is
  parsed and rendered inside the worker (`tairix_help`'s `HelpDoc::parse`
  plus the short/full renderers), and the parent-side `render_help`
  client re-parses the returned bytes through the `lib/vt` streaming
  parser, admitting only the closed op set a help render can contain
  (printable text, line feeds, the bold/underline SGR pairs) and
  re-encoding them canonically — the caller writes bytes its own process
  produced, never bytes the worker chose. A document-parse error crosses
  the boundary typed (`HelpError`, code for code), so diagnostics lose
  nothing to the isolation. `man` is the consumer: it locates and reads
  the document with its own file authority (`tairix_help::load_raw`),
  re-spawns itself as the worker (`CAP_PROC_SPAWN` in its manifest), and
  withholds the page — never falling back to an in-process parse — when
  the renderer fails.
- **Icon rasterisation** (`imagerender`): an application bundle's icon —
  SVG or PNG bytes shipped inside the bundle, not from the system — is
  sniffed, decoded, and rasterised to the caller's requested square side
  inside the worker (`tairix_svg`/`tairix_image`/`tairix_icon`/
  `tairix_raster`), and the parent-side `rasterise_icon` trusts nothing
  about the reply beyond its length and echoed side before handing the
  bytes to the compositor. A PNG source decodes through its own,
  tighter decode-time bounds (independent of the requested output side,
  so a small request cannot smuggle a huge source image past a small
  reply), is fitted inside the square preserving its aspect ratio, and is
  scaled through the crate's one shared resampler
  (`tairix_raster::resample`: a separable filtered resample in
  premultiplied-alpha space — the exact area integral when reducing, so a
  downscale blends instead of aliasing, and a Catmull-Rom cubic when
  enlarging, so an upscale is never a sample-and-hold); an SVG
  source rasterises directly through the shared vector-icon polygon-fill
  path. Either a typed refusal (unsupported format, a decode failure, or
  an unrenderable result) or a sandbox failure simply means the desktop
  session falls back to its own built-in glyph — never a crash.
- **Wallpaper placement** (also `imagerender`, the same worker): a
  desktop wallpaper — a shipped master or a file the user picked, never
  parsed outside the sandbox — is sniffed, decoded, and placed onto the
  session's screen size across three ops. `OP_WALLPAPER_PREPARE` decodes
  the source at the smallest scale its format offers that still covers the
  destination (`tairix_image::decode_fitted`, bounded by
  `MAX_WALLPAPER_DECODE_PIXELS`), so an 8.3-megapixel master bound for a
  1080p screen costs a quarter of its pixels rather than all of them, and
  a screen so large that no covering scale fits the bound is served from
  the largest scale that does rather than refused; it then computes its
  placement (`tairix_wallpaper::place`), holding the decoded source and its
  placement in the worker; `OP_WALLPAPER_BAND` draws and returns exactly
  the destination rows asked for; `OP_WALLPAPER_RELEASE` drops the held
  source. Bands exist purely to respect `MAX_FRAME`, never to raise it: a
  screenful of straight-alpha RGBA8 already exceeds it above 1080p, and
  `OP_WALLPAPER_PREPARE`'s reply names the row count a single
  `OP_WALLPAPER_BAND` reply can carry. The destination is bounded by
  `MAX_WALLPAPER_WIDTH`×`MAX_WALLPAPER_HEIGHT` (4K) on both sides of the
  seam, and the source byte length by `tairix_wallpaper::MAX_WALLPAPER_BYTES`.
  A tiled fit repeats the source at 1:1; every other fit resamples the
  placement's source rectangle into its destination rectangle through the
  same shared resampler the icon path uses. Wherever the placement does
  not cover the destination (a letterboxed fit, a source smaller than the
  screen), those pixels are left fully transparent — this service never
  draws the desktop's backdrop colour, only the wallpaper. The
  parent-side `render_wallpaper` drives the whole prepare/band/release
  sequence, validates every band's echoed geometry and exact pixel length
  fail-closed before trusting it, assembles the final buffer, and always
  releases the held source afterwards, on the success path and every
  error path alike, so a worker never holds a decoded wallpaper past one
  call. A later prepare on the same (reused) worker replaces whatever an
  earlier one left held; `OP_RASTERISE` keeps working unchanged whether or
  not it is interleaved with a wallpaper sequence on the same worker.

Host tests inject the in-process `loopback` fake exactly as the
`Fs`/`Tty` seams take fakes, so a consumer's full parent-side path runs
under plain `cargo test`.

## What this deliberately is not

- It is not a general jail configuration surface: there is exactly one
  sandbox shape, so review is over one list, not a policy language.
- It is not seccomp-style per-process filter state: the brand is a
  single kernel-side bit on the task's capability record, checked at
  the one existing dispatch checkpoint — no per-syscall filter tables,
  no new hot-path cost for non-sandboxed tasks beyond one boolean read.
- It adds no syscall and no privileged path: the flag rides the
  existing attach block and only ever narrows.

## Test coverage

- `lib/abi`: sandbox block round-trip; refusal of every ambient shape
  (inherit-form wires, uid switch, console index) and of reserved flag
  bits.
- `kernel/sec`: `as_sandboxed` strips all three sets; `delegate` and
  `apply_token` refuse a sandboxed target (empty payload included), and
  the refusals are audited.
- `kernel/syscall`: the allow-list is frozen exactly; an exhaustive walk
  of the whole `abi-v1` table proves every non-listed syscall is refused
  for a sandboxed caller before its handler runs, with the denial
  audited.
- `kernel/core`: an end-to-end spawn with a sandbox attach block admits
  a child whose record is sandboxed and empty despite a manifest that
  requests a capability, with every standard stream closed.
- `lib/sandbox`: framing round-trips and truncation/oversize refusals;
  serve-loop semantics; containment (typed error, reap, replacement,
  logged events, frozen event ids); fail-closed decode of hostile
  replies; the `helpdoc` render-op whitelist (forbidden escapes, OSC
  strings, colour SGRs, and truncated trailing escapes all refuse the
  whole reply) and typed `HelpError` round-trips; the `imagerender`
  icon service (an SVG icon rasterises to an exact uniform colour, a PNG
  icon downscales through the shared resampler to a hand-checked known
  average, aspect-fit letterboxing centres with transparent padding,
  every refusal shape, and a hostile reply's wrong tag/echoed side/pixel
  length/trailing bytes each refuse fail-closed); the `imagerender`
  wallpaper service (a round trip for each of the five fits with exact
  corner/centre pixels, a tiled repeat verified across a whole grid, a
  4K destination that must band across several replies assembling
  byte-identical to a uniform-colour reference, a band before a prepare,
  a band out of range, a zero-row band, an oversize destination, an
  oversize source, a malformed image, an unrecognised format, release
  fail-closing a subsequent band, and `OP_RASTERISE` still round-tripping
  after a wallpaper sequence on the same worker); and the `fuzz_sandbox`
  harness (hostile input files through the decode, helpdoc, and
  imagerender icon/wallpaper request decoders, hostile worker replies
  into every client decoder) in `cargo xtask fuzz`.
- `userland/apps/man`: the loopback-driven suite runs the real
  `HelpService` end to end, and hostile-renderer tests prove a
  disbelieved reply withholds the page (typed `ManError::Render`, no
  byte reaches the console) while `-h` degrades to the usage banner.
- QEMU (`tests/integration/sandbox_program` + `sandbox_qemu_aarch64`):
  the whole seam over the real syscalls on the `virt` board — decode of
  valid and malformed inputs through a genuinely sandboxed worker, real
  crash containment with a surviving caller, and the syscall wall probed
  from inside a live sandbox (`fs_open`/`spawn` denied while the pipe
  reply crosses).
