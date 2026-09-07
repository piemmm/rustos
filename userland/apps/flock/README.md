# `flock` — run a command holding an advisory file lock

Stability: **experimental**

The canonical userland consumer of the advisory byte-range lock ABI
(`plans/FILELOCK.md`): takes a whole-file lock, runs a command while
holding it, and exits with the command's status. Two runs naming the same
lock file never overlap, which is the mutual exclusion a shell script needs
without a lockfile protocol that can go stale.

## Shape

* `src/lib.rs` — the pure parse-and-run engine. Every outside effect is an
  injected seam (`Session`, `Output`, `tairix_help::HelpSource`), so the
  whole decision surface, including every exit code, is host-testable
  without a kernel.
* `src/run.rs` — the freestanding `Run` binary. Wires the real seams: a
  `tairix_rt::File` opened `READ|WRITE|CREATE` and locked through the
  `File::lock`/`try_lock`/`lock_timeout` methods, and the shared
  command-word resolution policy (`lib/cmdres`) for the command operand.
* `src/tests.rs` — the host tests.

The handle is held for the whole run: the lock lives on the open file
description, so releasing it is the process ending, not an unlock the tool
has to remember on every error path.

## Compatibility

Options follow util-linux `flock(1)`: `-s`, `-x`, `-n`, `-w`, `-E`, `-v`.
Three of its options are deliberately **absent rather than accepted and
ignored**, because there is no honest implementation of them here:

| Option | Why |
|---|---|
| `-u`, `--unlock` | Acts on a descriptor a shell opened beforehand (`exec 9>file`). There is no descriptor form here, and a lock releases when its description closes. |
| `-o`, `--close` | Same descriptor form; closing the handle would release the very lock the command needs held. |
| `-c` | Runs its argument through a shell. Spelled explicitly as `flock file elsh -c '…'`, which does not hide which shell runs. |

Asking for any of them is a usage error (exit `2`), so a script is told
rather than run without the lock it asked for.

## Capabilities

`CAP_FS_ACCESS` (open and lock the file, read the bundle's own `Help/`),
`CAP_CONSOLE_WRITE` (short help and diagnostics), `CAP_PROC_SPAWN` (launch
the command). No capability is needed for the lock itself beyond the one the
open already required.
