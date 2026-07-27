# Font service (`userland/system/fontd`)

`tairix-fontd` is the user-space service that owns the system's fonts and
renders text. Text rendering is a single, sandboxed OS resource (`AGENTS.md`
§16.4, §19.5): `fontd` is the **only** process that holds a font face or runs
the TrueType outline rasteriser. Every other process — the compositor, the
taskbar, a terminal, an app — draws text by asking `fontd` for a glyph's
coverage over the reserved `FONT_ENDPOINT`, through the thin
[`tairix-font`](../lib/font.md) client. The installed binary lives at
`/System/Services/fontd.app/Run`.

The crate is `no_std` and depends only on the audited `lib/*` crates
`tairix-abi`, `tairix-fontface`, `tairix-vt`, and `tairix-log`, so a userland
service never links a kernel or driver crate (`AGENTS.md` §17.4).

## Why a service

Older builds embedded the full-Unicode glyph atlas (~3.6 MB) and the four
TrueType faces (~6.1 MB) into **every** GUI consumer via `include_bytes!`, so
each `Run` image carried its own ~10 MB read-only copy. Launching a GUI app
read, hashed, and eagerly copied all ~10 MB, which was the reported slow
desktop launch — glacial under QEMU TCG. Moving the payload to one service
removes it from every app image and, at the same time, satisfies four charter
rules the old stack violated: fonts as a curated OS shared library (§16.4),
system fonts under `/System/Fonts` (§16.2), untrusted font parsing in a
minimum-capability sandbox (§19.5), and no data duplicated into every consumer
(§2.2, §2.3).

## The sandbox and its authority

`fontd` runs in its own address space as the dedicated `fontd` service account
(uid 15, primary group `services`). Its signed `AppInfo` requests exactly three
capabilities and its account ceiling grants exactly the same three:

- `CAP_IPC_BIND_PRIVILEGED` — to bind the reserved well-known `FONT_ENDPOINT`,
  so a squatter cannot claim the rendezvous first and feed forged glyph
  coverage to the compositor and every app.
- `CAP_FS_ACCESS` — for the one-shot startup read of the four committed
  `/System/Fonts` faces through the secured VFS, which still authorises every
  path per-inode under the service's attested identity. `/System` is mounted
  read-only, so this reach can never write.
- `CAP_LOG_EMIT` — for its structured audit records (the `17000` event range:
  `SERVICE_READY`, `SERVICE_UNAVAILABLE`).

It requests **no** spawn or network authority, and keeps no open descriptor
after the startup read — once parsed, the service retains only the in-memory
faces. The untrusted TrueType parse runs in this service's own isolated address
space, so even a malformed face — the classic font-parser attack surface —
faults only this sandbox, never a compositor or a terminal. Serving glyph
coverage needs no capability of its own: drawing text is not a security
boundary (§5.2), and the *reply path* still validates every field and fails
closed on a corrupt frame (§5.4).

## The dispatcher

The host-testable core is `FontService`, the rasterising dispatcher. It owns
the parsed faces and a bounded `(face, glyph, cell height)` FIFO coverage
cache, and turns one decoded `FontRequest` into a framed reply:

1. Resolve the requested Unicode scalar to the covering face —
   Latin/Greek/Cyrillic to Inconsolata EX, Japanese to M PLUS 1 Code, Korean
   to D2Coding, Hebrew to Noto Sans Hebrew, else U+FFFD.
2. Rasterise once at the requested cell height through the shared
   `tairix-fontface` engine (the 4-bit coverage engine scaled ×17 to the
   protocol's 8-bit samples — byte-identical to the atlas the old blitter
   produced), and memoise the result.
3. Emit the reply. `handle` **always** emits a reply, framing a status-word
   error frame on any failure so both the glyph and metrics clients decode a
   definite outcome (fail closed).

The face bytes are injected (borrowed) rather than embedded, so the
security-relevant rasterise-and-cache logic is exhaustively host-tested against
the committed repository faces with no on-disk `/System/Fonts`. The
`tairix-fontface` TrueType parser additionally carries its own fuzz harness
(`AGENTS.md` §19.6).

## The `FONT_ENDPOINT` protocol

The wire protocol lives in `tairix_abi::font_ipc`, modelled on the other
reserved-endpoint service protocols (`display_ipc`, `window_ipc`) and held to
the same ABI discipline as the syscall table (§9): versioned, hashed, and
frozen on the first release (mutable now — `abi-v1` is not frozen). It is not
part of the curated C-ABI surface, so the generated C headers are unchanged.

- A fixed 16-byte `FontRequest` in: `Glyph { scalar, cell_height }` or
  `Metrics { cell_height }`. The scalar is carried as a `char`, so a surrogate
  or out-of-range code point is unrepresentable in an accepted request; the
  cell height is bounded by `FONT_MIN_CELL_HEIGHT`/`FONT_MAX_CELL_HEIGHT`
  (8..=512) — a validation bound, not a capacity.
- A status-framed reply out: a glyph reply is `width`, `height`, `advance`, and
  the `width * height` 8-bit coverage samples (bounded by
  `FONT_MAX_GLYPH_REPLY`); a metrics reply is the monospace `FontMetrics`
  (`cell_width`, `cell_height`, `baseline`) for a client that holds no geometry
  of its own. One shared `glyph_coverage_len` bounds check governs both encode
  and decode, so producer and consumer cannot diverge.

## Startup and discovery

`fontd` ships as a signed `/System/Services/fontd.app` bundle — a service is an
app (§16.2, §16.5). It is **not** a boot-floor service: text rendering is only
needed by the graphical desktop, so a headless or text-only system never runs
it (headless-first, §17.3). Instead **`login` starts it** the first login round
a machine is display-capable (the desktop bundle is installed and a display
service is live). login is the natural owner: it holds `CAP_SPAWN_AS_USER` —
the authority the graphics-only `fontd` account (uid 15) needs and that neither
the shell nor the desktop app has — so it drops `fontd` onto its own service
account exactly as it drops a session onto the authenticated user. This covers
both ways the desktop is launched (a graphical login, or the shell's `desktop`
command) and starts `fontd` once per login process; a duplicate would fail
closed on the reserved-endpoint bind. A refused start is audited
(`FONTD_UNAVAILABLE`) and login proceeds — the desktop degrades to unrendered
text rather than failing (§2.24).

`login` spawns `fontd` by its path, which the kernel resolves through the same
program gate as any other program: from the verified on-disk
`/System/Services/fontd.app` bundle on the aarch64 production build, and from
the compiled-in program registry on x86_64/riscv64 until those ports' on-disk
storage floors land (`fontd` is a registered spawnable program on those ports,
not an init-auto-started boot service). The desktop's font client fails closed
until `fontd` has bound `FONT_ENDPOINT`, so the first frames may paint no text
and then fill in once the service is serving.

> Note: `fontd` deliberately is not started at boot because doing so exposes a
> latent early-boot concurrent-spawn kernel defect (tracked as D18 in
> `plans/OPEN-DEFECTS.md`); starting it from the post-boot graphical path both
> sidesteps that defect and is the headless-first-correct design.
