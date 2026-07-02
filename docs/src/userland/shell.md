# elsh — the default shell (`userland/shell/elsh`)

**elsh** ("Element Shell", crate `rustos-elsh`) is the default RustOS
command interpreter: a POSIX-ish shell that
reads a line of text and runs it. It lexes the line with full quoting and
escaping, parses pipelines and the `;`/`&&`/`||`/`&` connectors, expands
`$`-variables, runs a small set of builtins in-process, and launches
everything else through an injected process host with job control over
background and stopped jobs.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `lib/abi` — for the stable `Errno`
carried back by the process-host seam — so a userland program never links
a kernel or driver crate (`AGENTS.md` §17.4).

## A pure interpreter

The shell decides *what* to run and *with what arguments*; it never
itself touches the kernel or a terminal. The two operations that reach
the outside world are injected seams, mirroring `init`'s `Spawner`/
`Reaper` design ([PID 1 service manager](./init.md)):

- `ProcessHost` — `launch` / `wait` / `signal` / `poll` a job, and
  `change_directory`. Every failure is reported as the stable `Errno` the
  kernel returns, so the shell invents no parallel error vocabulary
  (`AGENTS.md` §2.2).
- `Console` — `write_stdout` / `write_stderr`.

On a running kernel these are syscall-backed; in tests they are in-memory
fixtures. Splitting the seams from the interpreter keeps every parsing,
expansion, and control-flow decision exhaustively testable without a
kernel.

## Run pipeline

`Shell::run_line` is the one entry point. For each line it:

1. **Reports finished background jobs.** It first drains the host's
   background state changes (`ProcessHost::poll`) into the job table and
   prints `[N] Done <cmd>` for any that finished, exactly as a shell does
   before its next prompt.
2. **Lexes** the text into a quoting-aware token stream
   (`lexer::tokenize`).
3. **Parses** the tokens into a `CommandList`: a sequence of pipelines
   joined by the `;`/`&&`/`||` connectors, each optionally ended by `&`
   (`parser::parse`).
4. **Runs** each list entry whose run-condition the previous `$?`
   satisfies — `&&` runs on success, `||` runs on failure, `;` always —
   expanding each word (`env::Environment::expand_word`) and either
   dispatching a builtin or launching through the `ProcessHost`.

## Redirections

The lexer decodes redirection operators into a `RedirOp` and the parser
attaches the target word the file-opening forms need, so a redirection is
understood before anything runs. The shell then *lowers* each parsed
`Redirection` into primitive `ResolvedRedirection { fd, action }` values — an
`Open { mode, target }`, a `Dup { source }`, or a `Close` — that the process
host applies in source order. The host never re-derives redirection meaning;
it only opens, duplicates, or closes a descriptor.

Supported operators (each may carry an explicit leading descriptor number, an
IO number, e.g. `2>`, `3>>`, `0<`):

- **File opens:** `<` (read), `>` / `>|` / `>!` (write, truncating; the `|`/`!`
  spellings override `noclobber`), `>>` / `>>|` / `>>!` (append), `<>`
  (read-write).
- **Duplication and close:** `n>&m`, `n<&m`, `2>&1` duplicate a descriptor;
  `>&-`, `<&-`, `n>&-` close one.
- **Combined stdout+stderr:** `&>` / `>&file` and the append (`&>>`, `>>&`) and
  clobber-override (`&>|`, `&>!`) spellings. These lower to an open on fd 1
  followed by a duplication of fd 1 onto fd 2 — the single definition of what a
  combined redirection means (`AGENTS.md` §2.2).
- **Here-string:** `<<< word` feeds the expanded word plus one trailing
  newline as the input of its descriptor (default fd 0). It lowers to a
  `HereString` action carrying those bytes — the single definition of the
  here-string's shape — so the host supplies them verbatim as a read backing.
  (The multi-line here-documents `<<` / `<<-` are not yet supported.)

A descriptor number is an IO number only when it is glued directly to a `<`/`>`
(so `echo 2` is a plain argument, but `2>err` names fd 2). The parser
**fails closed** (`AGENTS.md` §5.4, §2.9): a file redirection with no target,
an ambiguous duplication (`<&file`, `2>&file`), a here-string with no target,
or an as-yet-unsupported multi-line here-document (`<<`, `<<-`) runs
**nothing**.

### Target: resource reference vs. filesystem path

RustOS has no `/dev`, so the byte sinks and sources a redirection can name
(`sys:null`, `sys:zero`, `sys:random`, …) are **resource references**, not
device files. Each expanded `Open` target is classified into a `RedirTarget` —
`Path` or `Resource` — through the single shared `lib/resref` parser, never a
shell-private reference grammar (`AGENTS.md` §2.2). A target is a resource
reference only when it is a *relative* path whose first path component holds a
`:` preceded by a registered resource namespace and *not* immediately followed
by `/`, and whose prefix is neither `.` nor `..`:

| target           | resolves to                                             |
|------------------|---------------------------------------------------------|
| `sys:random`     | resource reference (registered namespace)               |
| `Home:/notes`    | path (alias-path form: `:` immediately followed by `/`) |
| `/sys:random`    | path (absolute)                                         |
| `./sys:random`   | path (first component is `.`)                           |
| `foo/sys:random` | path (first component `foo` has no `:`)                 |
| `foo:bar`        | path (`foo` is not a registered namespace)              |

The rule reserves nothing on disk: `:` is a legal filename byte on ext/POSIX
volumes, and a real file named `sys:random` stays reachable as `./sys:random`
or when quoted. A target whose spelling names a registered namespace but is
**not** a well-formed reference (`sys:null@`) fails the whole line closed — the
shell never falls back to creating a file, so a typo cannot silently write junk
to disk (`AGENTS.md` §5.4).

Not yet implemented (tracked in `plans/SHELL.md`, deliberately failing closed
rather than misbehaving): multi-line here-documents (`<<`, `<<-`), process
substitution (`<(…)`, `>(…)`, `=(…)`), zsh multios fan-out, and dynamic
descriptor allocation (`{var}>`). Classifying a target is done; *resolving* a
`Resource` target to a kernel stream backing (opening `sys:null`, the
capability-checked resolve of any other namespace) waits on the same launch
ABI that gates applying a file redirection. The current runtime process host
still carries only a program path, so it reports `NotImplemented` for any
redirection — a `Path` and a `Resource` target alike — until the launch ABI is
extended; the parsing, classification, lowering, and fail-closed semantics
above are exercised in full against the in-memory fixtures.

## The session program (`Run`) and its REPL

The crate is both the interpreter library above and the `Run` entry-point
binary of the `Shell` application bundle (`AGENTS.md` §16.5) — the program
PID 1 [`init`](./init.md) launches as the user's session. The binary is a
pure-Rust program (`AGENTS.md` §1): it links `rustos-rt` (`_start`, the stack
canary, the panic handler, the `mem_map`-backed global allocator, and the
syscall wrappers), never the C ABI.

`run::main` wraps the interpreter in a **read-eval-print loop** (`repl::run`)
over the program's **inherited standard streams** (`AGENTS.md` §20):

- It reads command lines from **standard input** (fd 0) through
  `rustos_rt::stdin`, reassembling lines across reads and stripping a
  trailing CRLF.
- It writes the prompt and all command output to **standard output** (fd 1)
  and **standard error** (fd 2) through the `RtConsole` seam.
- It emits advisory metadata on the **standard information stream** (fd 3,
  `AGENTS.md` §20.1) — currently a single `omission` record when an input
  line exceeds the 4 KiB limit and is discarded.

The loop binds to fd 0/1/2/3 **only**, never a console, UART, or framebuffer:
naming a device would be ambient authority (`AGENTS.md` §4) and hidden
coupling (§17.3 / §17.4), and the same binary must work whatever the spawner
backed the streams with (§20 — device independence is a property of the
stream layer). A zero-length read is treated as end of input (a clean session
end); *blocking* until a byte arrives is the stream backing's job, not the
program's — kernel-core's `BlockingConsoleRead` parks an empty-handed
`stream_read` caller until input arrives (`plans/PI.md` P6e-2), so an
interactive session sits at its prompt waiting for the user to type.

The `RtProcessHost` launches external commands through the `spawn` syscall
and reaps them through `wait`. The current `spawn` ABI carries only a program
*path* — no argument vector, environment, pipe, or redirection — so the host
launches a single bare-path command and fails closed with `NotImplemented`
on anything it cannot yet express (a pipeline, a redirection, arguments,
`fg`/`bg` signals, or `cd`); richer launches await an ABI extension. The
in-process builtins (`echo`, `exit`, `export`, `pwd`, `help`, …) work
regardless.

## Builtins

`cd`, `pwd`, `exit`, `export`, `unset`, `echo`, `jobs`, `fg`, `bg`,
`help`. A builtin runs inside the shell process because it mutates shell
state — the environment, the working directory, the job table, or the
`exit` request a read-eval loop watches; everything else is launched as
an external program.

## Job control

A backgrounded pipeline (`&`) is added to the `JobTable` as a running
job and its `[N] pid` line is printed; `$?` is `0`. A foreground job that
the host reports as `Stopped` becomes a stopped job. `fg` and `bg`
resume a job by sending `Signal::Continue` through the host, `fg` then
waiting on it. Finished background jobs are reported lazily, before the
next line, so output never interleaves mid-command.

## Failure handling (`AGENTS.md` §2.9)

`ParseError` is the shell's only *line-aborting* error — a lexical fault
(unterminated quote, dangling escape) or a grammatical one (empty
command, a redirection with no target, an unterminated `${...}`). A line
that does not parse or expand runs **nothing** and sets `$?` to `2`.

Everything that goes wrong *after* a line is understood — a program that
cannot be launched, a `change_directory` denial — is an ordinary
non-zero exit status (e.g. `127` for a command that will not launch),
never a panic and never a line abort, so the remaining connectors behave
as POSIX requires.

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
grammar errors, `$`-expansion, every builtin, foreground status
propagation, the command-not-found path, background job tracking, the
`Done`-before-prompt reporting of finished jobs, and connector
short-circuiting.
