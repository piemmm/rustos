# `lib/tty` — the shared tty line discipline

**Stability: experimental.**

One, sink-agnostic definition of the terminal line discipline TAIRiX cooks
bytes with, driven by both the kernel console device (`kernel/core::console`)
and the pseudo-terminal (`plans/PTY.md`) so neither carries a private copy of
the cooking rules (the charter forbids that duplication).

It owns no I/O: every entry point takes the caller's byte sink as a closure.

- `write_cooked(bytes, sink)` — output `ONLCR` translation (a bare `LF`
  becomes `CR LF`), preserving the POSIX short-write contract; the sink is
  fallible and may short-write.
- `EchoLine::echo(bytes, emit)` — input local echo: `CR`/`LF` → `CR LF`,
  bounded Backspace/Delete rub-out, and split Delete `CSI 3 ~` recognition;
  the sink is best-effort (echo is cosmetic).
- `read_bounded(out, next)` — the input read bound: take at most one line
  from the caller's queue, delimiter included, leaving everything behind it
  queued. A terminal's queued input belongs to the terminal, not to whichever
  process reads first: a reader that took the type-ahead past its own line
  would own those keystrokes privately and lose them the moment it handed the
  terminal on (a login launching the session shell, a shell running a
  foreground child). Both terminals read under this bound — the console's
  type-ahead ring and the pty slave end.
- `job_control_signal(byte)` / `is_line_delimiter(byte)` — the pure
  classifiers: the cooked-mode `^C`/`^Z` → `Signal` mapping (the caller owns
  the policy and the delivery), and the one definition of what ends a line,
  shared by the echo, secret-marker, and read-bound paths.

The control-byte constants and the Delete-escape recogniser
(`tairix_vt::EraseSeq`) are `lib/vt`'s single definition; this crate is the
*assembly* of them, never a second copy of that vocabulary.

`no_std`, no `unsafe`, fail-closed, never panics; rustdoc on every public
item; host unit tests in `src/tests.rs`.
