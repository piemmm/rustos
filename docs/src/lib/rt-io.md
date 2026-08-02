# `tairix-rt::io` — userland I/O abstraction

`tairix_rt::io` is TAIRiX's ergonomic userland I/O layer: the counterpart of
`std::io` for a first-party Rust program. It is a **pure-Rust convenience layer**
over the existing `abi-v1` `stream_read` / `stream_write` traps — it adds **no**
syscall, capability, or `lib/abi` type, and reaches no authority a program does
not already hold. An I/O object only ever names a descriptor the kernel already
gave the process; it never names a device (the standard-stream rule).

The staged plan is `plans/IO.md`. Stability tier: **experimental**
(`abi-v1` is not frozen; the surface grows as callers need it).

## One vocabulary, no duplication

There is exactly one `Read` / `Write` definition for the whole of userland, so
no program re-implements the short-write loop or "read until newline" logic
(`AGENTS.md` §2.2):

- `Read` provides the primitive `read`, the transfer loop `read_fill` (read
  until the buffer is full or the input ends, reporting how much arrived), and
  `read_exact` on top of it.
- `Write` provides the primitive `write`, the transfer loop `write_drain` (write
  until the buffer is drained or the sink stalls, reporting how much was taken),
  `write_all` on top of it, a `flush`, and `write_fmt` (so `write!` / `writeln!`
  work), rendering through a fixed adapter that surfaces a formatting failure as
  a typed `Error::Fmt` rather than a panic.

`read_fill` and `write_drain` are **the** two transfer loops in userland.
`read_exact`, `write_all`, and `File`'s positional `read_at` / `write_at` are
all expressed over them rather than carrying their own copy, so a short-read or
short-write bug can only exist in one place.

Every fd backing shares this one vocabulary. The four standard streams
(`Stdin`, `Stdout`, `Stderr`, `StdInfo`), a `Stream` over an arbitrary
descriptor, and an owning `File` — whether it is a path, a resource reference, a
pipe end, or a pty end — all go through the **identical** code path: the shared
`stream_read` / `stream_write` primitives. There is no separate "file I/O"
trait, because the kernel resolves every descriptor the process holds through
one table.
The module also carries the one `write_stderr_line` helper every command app's
`Run` binary reports diagnostics through (best-effort, never the data stream),
so the line-to-fd-2 loop is written once.

## fd-generic: one borrowed view, one owning handle

`Stream::new` views a descriptor the process already owns (a standard stream, or
a file / pipe / tty / resource-reference fd a spawner wired in or a subsystem
opened). It is **borrowed** and does not close the descriptor.

`File` is the **owning** handle and releases its descriptor on drop, whatever
the backing — the close trap is descriptor-generic, so one owner type covers
paths, resource references, pipe ends, and pty ends alike. A second owning fd
type alongside it would be the duplication this layer exists to prevent, so
there is no `OwnedStream`.

Obtaining a *new* fd — opening a file under a capability, resolving a resource
reference, creating a pipe — is owned by the filesystem and resource-reference
subsystems, not this layer; the trait module exposes no `open` / `resolve` and
so cannot widen authority.

## Sequential and positional

`File`'s `Read` / `Write` are **sequential**: they transfer at the shared
open-file-description cursor and advance it, so successive reads walk the file
and two descriptors cloned from one description (a spawn wire, a delegation)
interleave at one position instead of overwriting each other. This is the same
`stream_read` / `stream_write` trap the standard streams use, which is why a
file, a pipe, and a terminal are indistinguishable to a program that just wants
bytes.

`File::read_at` / `write_at` are **positional**: they take an explicit byte
offset and leave the shared cursor untouched, so two positional callers of one
description never contend over a position. The kernel serves both from a single
descriptor I/O path parameterised only by where the position comes from, so the
direction gate, capability checks, and copy boundaries cannot drift between
them.

## Buffering

- `BufWriter` coalesces many small writes into a single underlying write. Its
  buffer is a fixed-capacity inline array (no heap allocation), flushed when
  full, on an explicit `flush`, and best-effort on drop. A write at least as
  large as the buffer bypasses it and goes straight through, untorn.
- `BufReader` buffers reads and offers line-oriented reading for a REPL:
  `read_until`, `read_line`, and a `lines` iterator that strips the trailing
  `\n` (and a preceding `\r`).

The buffer capacity is a const generic (`CAP`) defaulting to
`DEFAULT_BUF_CAPACITY` (4096 bytes).

## `stdinfo` (fd 3) semantics

`StdInfo`'s `Write` is best-effort: fd 3 is optional and ignorable (there may be
no consumer), so it reports the buffer fully consumed regardless of how many
bytes the kernel accepted. It never surfaces a short write that could stall
`write_all` or an error a program depends on (`AGENTS.md` §20.1). The structured
`StdInfoRecord` framing itself lives in `lib/abi`; this layer only carries the
bytes.

## Fail closed, fail loud

No path panics or uses `unwrap` / `expect`. A short read or write is looped over
by the provided helpers, `write_all` fails closed with `Error::WriteZero` if a
sink stops accepting bytes (never an infinite loop), and `read_exact` fails
closed with `Error::UnexpectedEof`.

A **kernel refusal is never disguised as end-of-input.** `Error::Os` carries the
kernel's own `Errno` — a descriptor that is not open in the requested direction,
a missing capability, a broken pipe, a faulted buffer, an elapsed read bound —
so `Ok(0)` from a read means end-of-input and nothing else. This matters because
the universal shape of a consumer is "read until it returns zero": folding a
failure into a zero-length read would make a revoked capability look like a
complete input and let the consumer silently truncate what it processed, which
is precisely the quiet, wrong-answer failure the charter's fail-loud rule
forbids. `Error::as_errno` converts back for an interface that speaks the
kernel's vocabulary, keeping the kernel's code when there is one; a condition
this layer raised on its own reports `NotImplemented` rather than borrowing an
unrelated code that would misdescribe the kernel.

`Stream::read_timeout` / `Stdin::read_timeout` are the bounded companions of
`read`, so a full-screen program parks on its input and still refreshes on a
cadence instead of busy-polling; an elapsed bound arrives as
`Error::Os(Errno::TimedOut)` and is therefore distinguishable from a dead
console.

## Not a log path, not a C `stdio`

Structured and audited log *records* travel through `lib/log`, never these
traits; a `log`-viewing tool renders its text to the standard streams through
this layer like any other program. TAIRiX ships no system-wide C `stdio`; the
C-ABI runtime class stays minimal, and a third-party C program brings its own
libc in its app bundle.
