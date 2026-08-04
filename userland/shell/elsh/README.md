# `tairix-elsh` — elsh (Element Shell), the default TAIRiX shell

Stage 6 deliverable (`AGENTS.md` §3 `userland/shell/`). elsh (Element
Shell) is a POSIX-ish
command interpreter: it lexes a line with full quoting and escaping,
parses pipelines (`|`, `|&`, the `!` status negation) with the
`;`/`&&`/`||`/`&` connectors and `NAME=VALUE` prefix assignments, expands
`$`-variables, runs a small set of builtins in-process, and launches
everything else through an injected process host with job control over
background and stopped jobs. Redirections cover the POSIX and zsh operator
families — including multios fan-out/concatenation and `{var}`
dynamic-descriptor allocation (≥ 10, never a standard stream); process
substitution and the `( … )`/`{ …; }` compound commands are recognised and
fail closed rather than misparsing (tracked in `plans/SHELL.md`).

On a terminal-backed session the REPL runs an interactive line editor
(`src/editor.rs`) in the raw read discipline: arrow-key history with an
incremental `Ctrl-R` reverse search, the readline movement/kill/yank
chords, `Ctrl-C` line cancel, `Ctrl-D` end-of-input, and Tab completion
(`src/complete.rs`) over command names, paths, and resource references —
see `docs/src/userland/shell.md`. A backing that refuses raw mode (a
pipe, a script) keeps the plain line reader, byte-identical.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependencies are the audited `lib/*` crates
[`tairix-abi`](../../../lib/abi) (the stable `Errno` on the host seam),
[`tairix-cmdres`](../../../lib/cmdres) (the one command-word search policy,
which completion enumerates), [`tairix-complete`](../../../lib/complete)
(the one path-candidate completion policy, which the shell dresses in its
escaping and candidate classes), [`tairix-curses`](../../../lib/curses)
(the shared key-event decoder), [`tairix-resref`](../../../lib/resref)
(the one resource-reference spelling parser and registry), and
[`tairix-vt`](../../../lib/vt) (the shared terminal vocabulary the editor
renders through and the plain reader's line discipline), so the shell
never links a kernel or driver crate (`AGENTS.md` §17.4).

## A pure interpreter

`tairix-elsh` decides *what* to run and *with what arguments*; it never
itself touches the kernel or a terminal. The two operations that reach
the outside world are injected seams:

- `ProcessHost` — launch / wait / signal / poll a job, and
  `change_directory`. Failures come back as the stable `Errno` the kernel
  returns, so the shell invents no parallel error vocabulary
  (`AGENTS.md` §2.2).
- `Console` — write stdout / stderr.
- `LimitStore` — read / impose the calling process's resource limits, the
  seam the `ulimit` builtin drives (`AGENTS.md` §24.3). Backed by
  `tairix_rt::rlimit_get`/`rlimit_set` on a running kernel; a shell built
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
4. `Shell::run_line` — run each pipeline, honouring connectors, `!`
   negation, and the background flag, splitting prefix assignments into the
   child's environment (temporary around a builtin), lowering redirections
   to host primitives (merging zsh multios, allocating `{var}` dynamic
   descriptors), dispatching builtins or launching through the
   `ProcessHost`, and tracking jobs in the `JobTable`. (`Shell::parse_line`
   and `Shell::run_list` are the two halves the REPL drives separately to
   collect here-document bodies in between.)

## Builtins

`cd`, `pwd`, `exit`, `export`, `unset`, `echo`, `jobs`, `fg`, `bg`,
`ulimit`, `elevate`, `help`. A builtin runs inside the shell process
because it mutates or reads shell-side state (the environment, the
working directory, the job table, the exit request, the process's own
resource limits, or — for `elevate` — the controlling terminal its
echo-off password prompt must own); everything else is launched
externally.

`elevate <user> <program>` (`plans/CAPABILITY_USE.md` CU5) posts one
synchronous IPC call to this console's login supervisor, which
re-authenticates the target account and runs the program as it; the exit
code becomes `$?`. Driven through the fail-closed `Elevator` seam
(`host.rs`), backed in the `Run` binary by `self_origin` + `ipc_call`;
the shell itself holds no elevation authority and zeroises the password
buffer on every path.

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
status, never a panic and never a line abort, so `;`, `&&`, and `||` behave
as POSIX requires. A launch refusal follows the POSIX convention: `127`
("command not found") when resolution exhausted every candidate, `126` for
a command that resolved but cannot run (a permission or capability denial).

## Command resolution

A command word resolves through the pure candidate policy
`tairix_cmdres::resolution_candidates` (`lib/cmdres`, shared with the `man`
command's bundle lookup and owned by `plans/APPS.md` §8–§9):
an explicit path (containing `/`) bypasses the search, a trailing `.app`
names the bundle and runs its `Run` binary, and a bare word searches a
fixed prefix — the `/System/Commands/` and `/System/Applications/` system
stores then the user's own `<home>/Commands/` and `<home>/Applications/`
(all spelled once in `lib/abi`), which no `PATH` can reorder or override —
and then the alias-aware `:`-split `PATH`, in order. The runtime host
attempts the candidates in order — the kernel's byte-exact `spawn`
answering `NotFound` moves to the next — and the kernel authorises every
launch; a candidate list grants nothing.

## Deliberate simplifications

These keep a first shell small and predictable; each is documented where
it lives rather than papered over (`AGENTS.md` §2.1, §2.3):

- Expansion does not field-split or remove empty results: each word
  becomes exactly one argument.
- The supported expansions are `$NAME`, `${NAME}`, and `$?`. Spellings of
  unimplemented expansions stay inert word text except where running them
  would change command meaning — process substitution and compound commands
  fail closed with a parse error.
- A redirection on a builtin fails closed with status 1: builtins write
  through the `Console` seam, and silently sending a redirected stream to
  the terminal would be worse than refusing.

## Tests

`cargo test -p tairix-elsh` drives the interpreter against in-memory
`Console`/`ProcessHost` fixtures, covering the lexer's quoting and escape
rules, the parser's pipelines/redirections/connectors and its fail-closed
grammar errors, `$`-expansion, here-document collection (quoting, `<<-`
tab stripping, source order, the size bound, and the fail-closed paths),
every builtin, foreground status propagation, the command-not-found path,
background job tracking, the `Done`-before-prompt reporting of finished
jobs, connector short-circuiting, `!` negation, `|&` lowering,
prefix-assignment scoping, multios merging (and its fail-closed
mixed-direction case), `{var}` dynamic-descriptor allocation/close, and the
fail-closed process-substitution and compound-command spellings.
