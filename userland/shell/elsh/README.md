# `rustos-elsh` — elsh (Element Shell), the default RustOS shell

Stage 6 deliverable (`AGENTS.md` §3 `userland/shell/`). elsh (Element
Shell) is a POSIX-ish
command interpreter: it lexes a line with full quoting and escaping,
parses pipelines and the `;`/`&&`/`||`/`&` connectors, expands
`$`-variables, runs a small set of builtins in-process, and launches
everything else through an injected process host with job control over
background and stopped jobs.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependencies are the audited `lib/*` crates
[`rustos-abi`](../../../lib/abi) (the stable `Errno` on the host seam),
[`rustos-resref`](../../../lib/resref) (the one resource-reference spelling
parser), and [`rustos-vt`](../../../lib/vt) (the shared read line discipline
the REPL's line reader runs), so the shell never links a
kernel or driver crate (`AGENTS.md` §17.4).

## A pure interpreter

`rustos-elsh` decides *what* to run and *with what arguments*; it never
itself touches the kernel or a terminal. The two operations that reach
the outside world are injected seams:

- `ProcessHost` — launch / wait / signal / poll a job, and
  `change_directory`. Failures come back as the stable `Errno` the kernel
  returns, so the shell invents no parallel error vocabulary
  (`AGENTS.md` §2.2).
- `Console` — write stdout / stderr.
- `LimitStore` — read / impose the calling process's resource limits, the
  seam the `ulimit` builtin drives (`AGENTS.md` §24.3). Backed by
  `rustos_rt::rlimit_get`/`rlimit_set` on a running kernel; a shell built
  without one fails closed (`ulimit` reports `NotImplemented`).

On a running kernel these are syscall-backed; in tests they are in-memory
fixtures. This mirrors `init`'s `Spawner`/`Reaper` design and keeps every
parsing, expansion, and control-flow decision exhaustively testable
without a kernel.

## Pipeline

1. `lexer::tokenize` — text to a quoting-aware token stream.
2. `parser::parse` — tokens to a `CommandList` tree. A here-document
   (`<<`, `<<-`) parses *pending*: its body is collected from the following
   input lines (`CommandList::feed_here_doc_line`, bounded and fail-closed)
   before the list runs.
3. `env::Environment::expand_word` — `$`-expansion of each word.
4. `Shell::run_line` — run each pipeline, honouring connectors and the
   background flag, dispatching builtins or launching through the
   `ProcessHost`, and tracking jobs in the `JobTable`. (`Shell::parse_line`
   and `Shell::run_list` are the two halves the REPL drives separately to
   collect here-document bodies in between.)

## Builtins

`cd`, `pwd`, `exit`, `export`, `unset`, `echo`, `jobs`, `fg`, `bg`,
`ulimit`, `help`. A builtin runs inside the shell process because it
mutates or reads shell-side state (the environment, the working
directory, the job table, the exit request, or the process's own resource
limits); everything else is launched externally.

`ulimit [-a] [-H | -S] [<resource> [<value>]]` reports and imposes the
process's own resource limits (`AGENTS.md` §24.3) over the `LimitStore`
seam: `-a` (or no operand) lists every resource, `-H`/`-S` select the
hard/soft bound, and a `<value>` (a decimal or `unlimited`) sets it.
`<resource>` is a canonical `LimitKind` name (`address-space-bytes`,
`open-streams`, `processes`, `stack-bytes`). Lowering a bound is free;
raising a hard bound is gated kernel-side on `CAP_RLIMIT_RAISE` and a
denial is reported, never hidden (§2.9).

## Failure handling

`ParseError` is the only *line-aborting* error: a lexical fault
(unterminated quote, dangling escape), a grammatical one (empty
command, redirection with no target, unterminated `${...}`), or a
here-document whose body is unterminated or over-length. A line that
does not parse or expand runs **nothing** and sets `$?` to `2`.

Everything that goes wrong *after* a line is understood — a program that
cannot be launched, a `change_directory` denial — is an ordinary non-zero
status (e.g. `127` for a command that will not launch), never a panic and
never a line abort, so `;`, `&&`, and `||` behave as POSIX requires.

## Deliberate simplifications

These keep a first shell small and predictable; each is documented where
it lives rather than papered over (`AGENTS.md` §2.1, §2.3):

- Expansion does not field-split or remove empty results: each word
  becomes exactly one argument.
- `NAME=VALUE` is an assignment only when the whole simple command is
  assignments; it is not a per-command temporary-environment prefix.
- The supported expansions are `$NAME`, `${NAME}`, and `$?`.

## Tests

`cargo test -p rustos-elsh` drives the interpreter against in-memory
`Console`/`ProcessHost` fixtures, covering the lexer's quoting and escape
rules, the parser's pipelines/redirections/connectors and its fail-closed
grammar errors, `$`-expansion, here-document collection (quoting, `<<-`
tab stripping, source order, the size bound, and the fail-closed paths),
every builtin, foreground status propagation, the command-not-found path,
background job tracking, the `Done`-before-prompt reporting of finished
jobs, and connector short-circuiting.
