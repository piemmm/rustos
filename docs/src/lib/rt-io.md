# `rustos-rt::io` — userland I/O abstraction

`rustos_rt::io` is RustOS's ergonomic userland I/O layer: the counterpart of
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

- `Read` provides the primitive `read` plus a looping `read_exact`.
- `Write` provides the primitive `write` plus looping `write_all`, a `flush`,
  and `write_fmt` (so `write!` / `writeln!` work), rendering through a fixed
  adapter that surfaces a formatting failure as a typed `Error::Fmt` rather than
  a panic.

Every fd backing shares this one vocabulary. The four standard streams
(`Stdin`, `Stdout`, `Stderr`, `StdInfo`) and a `Stream` over an arbitrary
descriptor go through the **identical** code path — the shared `stream_read` /
`stream_write` primitives. When files, pipes, tty backings, or resource
references land, they reuse this layer instead of forcing a second I/O surface.
The module also carries the one `write_stderr_line` helper every command app's
`Run` binary reports diagnostics through (best-effort, never the data stream),
so the line-to-fd-2 loop is written once.

## fd-generic, non-owning

`Stream::new` views a descriptor the process already owns (a standard stream, or
a pipe / tty / resource-reference fd a spawner wired in). It is **non-owning**
and does not close the descriptor: `abi-v1` has no generic descriptor-close
trap, so an fd-generic close-on-drop handle would be a speculative interface
bound to a syscall that does not exist. Descriptor lifetime is the concern of
whichever subsystem minted the fd (the filesystem's own `File` closes its
descriptor on drop). Obtaining a *new* fd — opening a file under a capability,
resolving a resource reference — is owned by the filesystem and
resource-reference subsystems, not this layer; it exposes no `open` / `resolve`
and so cannot widen authority.

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

## Fail closed

No path panics or uses `unwrap` / `expect`. A short read or write is looped over
by the provided helpers, end-of-input is reported honestly as a zero-length
read, `write_all` fails closed with `Error::WriteZero` if a sink stops accepting
bytes (never an infinite loop), and `read_exact` fails closed with
`Error::UnexpectedEof`.

## Not a log path, not a C `stdio`

Structured and audited log *records* travel through `lib/log`, never these
traits; a `log`-viewing tool renders its text to the standard streams through
this layer like any other program. RustOS ships no system-wide C `stdio`; the
C-ABI runtime class stays minimal, and a third-party C program brings its own
libc in its app bundle.
