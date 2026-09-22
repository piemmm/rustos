# `tairix-sandbox` — the parser-sandbox seam

`tairix_sandbox` (`lib/sandbox`) is TAIRiX's one user-space seam over the
kernel's parser-sandbox spawn mode
([the parser sandbox](../security/sandbox.md)): the typed request/reply
path every program runs a parser of untrusted input through. The kernel
primitive makes a minimum-capability worker process *exist*; this crate
makes it *usable* — and writes the containment discipline exactly once,
so no program re-derives it.

Stability tier: **experimental**.

## Shape

- **`proto`** — a deliberately tiny length-framed byte protocol over any
  bidirectional `Channel` (pipes in production, in-memory fakes in host
  tests). Frames are bounded by `MAX_FRAME` — a fixed validation bound on
  a hostile peer, not a growable capacity — and both sides refuse an
  oversize declared length *before* reading or allocating a payload byte.
  End-of-stream on a frame boundary is the clean end of a conversation;
  end-of-stream inside a frame is a typed failure, never silently
  shortened data.
- **`worker`** — the serve loop a sandboxed process runs: read a request
  frame, hand the payload to a total `Service`, write the reply frame,
  finish when the parent closes the request stream. A `Service` encodes
  "that request is malformed" as a typed error reply; the loop itself
  cannot be derailed by request content.
- **`host`** — the calling program's side. `ParserSandbox::request` sends
  one payload and blocks for the reply. Every worker failure — crash,
  protocol violation, oversize reply, exit without answering — runs one
  containment path: the caller receives a typed `SandboxError`, the dead
  worker is disposed of (reaped) and **replaced**, and the event is
  logged with a stable id (`EventId(6000)` crashed, `EventId(6001)`
  unavailable; the crate owns the `6000..7000` range). Dropping the seam
  disposes of its live worker.
- **`session`** — the **duplex, long-lived** seam beside that one-shot
  pair, for a worker that serves a protocol rather than answering a
  question. Three things differ and each is load-bearing: many frames are
  in flight each way (one inbound frame may be answered with none or
  several); the parent never blocks, driving every transport operation
  from its own wait-set over the two descriptors the transport reports
  (`Stream` on the reply end, `StreamRoom` on the request end), so one
  session can never stall another; and a failed worker ends the session
  rather than being replaced, because it held the connection's protocol
  state. Containment is one path — dispose, log `EventId(6002)`, latch,
  and refuse every later call without touching the transport. Both queues
  are bounded by `SessionBounds` and committed at admission, so the
  steady state allocates nothing and the cost of a session is known
  before it is admitted; the send ceiling is derived from the outbound
  bound, which is what makes `OutboundFull` provably transient (an
  accepted payload always fits an empty queue) and `FrameTooLarge`
  permanent. `wants_read` going false is total back-pressure: the owner
  disarms, the pipe fills, and the kernel blocks the worker.
  Deadlock-freedom is structural, not argued: the sandbox allow-list
  leaves the pipe as the worker's only wake source, so the worker may use
  the ordinary blocking `Channel` while the parent's two readiness legs
  guarantee it is always woken. The worker side is `serve_session` over a
  `SessionService`, emitting through `FrameOut` straight to the channel,
  so fanning one frame out to many allocates nothing per frame.
- **`loopback`** — the public in-process fakes: for the one-shot seam each
  "worker" is a fresh `Service` run inline; `LoopbackSession` is the same
  for the duplex one, running a `SessionService` behind a
  `SessionTransport`. Either way a consumer's host tests drive the full
  parent-side path (framing, containment, typed decode) under plain
  `cargo test`, exactly as the `Fs`/`Tty` seams take fakes.
- **`decode`** — the first consumers behind the seam: executable-container
  summaries through [`tairix-binfmt`](./binfmt.md) and per-window
  instruction disassembly through [`tairix-disasm`](./disasm.md). The
  `DecodeService` runs inside the worker; the client helpers
  (`container_summary`, `manifest_summary`, `disassemble`) marshal typed
  requests and validate every reply field **fail-closed** — a worker that
  has parsed hostile bytes is itself hostile, so list counts, name
  lengths, tags, and instruction lengths are all bounds-checked before
  the caller acts on them, and truncation is reported honestly through
  `regions_truncated`/`symbols_truncated`, never silently.
- **`helpdoc`** — the sandboxed help-document render: the `HelpService`
  worker parses and renders a foreign bundle's document through
  [`tairix-help`](./help.md), and the client `render_help` re-parses the
  reply through the `tairix-vt` streaming parser, admitting only the
  closed op set a help render can contain (printable text, line feeds,
  the bold/underline SGR pairs) and re-encoding it canonically — a
  forbidden escape or a truncated trailing sequence refuses the whole
  reply, and a document-parse error round-trips typed (`HelpError`, code
  for code). `man` is the consumer: it reads the document with its own
  file authority (`tairix_help::load_raw`) and never parses it
  in-process.
- **`rt`** (feature `program`, freestanding targets only) — the
  production transport. `RtLauncher` spawns the program's **own binary**
  in a worker role: two fresh pipes wired to the child's fd 0/1 through
  `SpawnAttach::sandbox`, the shared `--parser-sandbox-worker` argv
  marker, and a blocking reap on disposal. The worker side
  (`worker_role` + `serve_stdio`) serves over fd 0/1 — exactly the
  surface the kernel sandbox allow-list admits. `RtSessionChannel` is the
  duplex transport over that same spawn (one shared pipe-pair-and-attach
  path, `--sandbox-session-worker`), reporting its two descriptor numbers
  so the owner can register them, and `session_worker_role` +
  `serve_session_stdio` are its worker half.

## Security posture

The seam adds no authority: the worker holds only the two pipe ends its
parent wired at spawn, and the kernel enforces the capability-empty brand
and the syscall allow-list. Nothing a worker replies is trusted beyond
the frame bound and the typed field validation, and the request payloads
never carry secrets or capability tokens.

## Testing

Unit tests cover the framing (round-trips, every truncation point,
oversize both ways), the serve loop, the containment discipline (typed
error, reap, replacement, logged events, frozen event ids, Drop
disposal), and hostile-reply refusal. The session seam adds its own:
ordered duplex exchange, a frame split across reads, fan-out, both send
refusals leaving the queue byte-identical, the readiness transitions in
each direction, clean end-of-stream against mid-frame truncation, an
oversize worker declaration refused before it is copied, containment
(disposed, logged, latched, never replaced), and a ten-thousand-round-trip
run asserting neither queue grows. The `fuzz_sandbox` harness (in
`cargo xtask fuzz`) drives mutated containers, pure noise under every
ISA, a hostile worker framing noise as replies, and the session's inbound
codec over the same noise a byte-run at a time — all through the public
client path. The aarch64 QEMU vertical
(`tests/integration/sandbox_program` + `sandbox_qemu_aarch64`) proves the
whole seam over the real syscalls: sandboxed decode of valid and
malformed inputs, real-process crash containment with a surviving caller,
the syscall wall probed from inside a live sandbox, and a duplex session
driven entirely from a wait-set that pushes twice a pipe's worth of frames
at a worker answering none of them — which completes only if the
`StreamRoom` wake fires.
