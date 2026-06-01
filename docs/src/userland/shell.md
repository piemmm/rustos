# Default shell (`userland/shell/shell`)

`rustos-shell` is the default command interpreter: a POSIX-ish shell that
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

`cargo test -p rustos-shell` drives the interpreter against in-memory
`Console`/`ProcessHost` fixtures, covering the lexer's quoting and escape
rules, the parser's pipelines/redirections/connectors and its fail-closed
grammar errors, `$`-expansion, every builtin, foreground status
propagation, the command-not-found path, background job tracking, the
`Done`-before-prompt reporting of finished jobs, and connector
short-circuiting.
