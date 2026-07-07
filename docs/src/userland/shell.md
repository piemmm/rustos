# elsh — the default shell (`userland/shell/elsh`)

**elsh** ("Element Shell", crate `rustos-elsh`) is the default RustOS
command interpreter: a POSIX-ish shell that
reads a line of text and runs it. It lexes the line with full quoting and
escaping, parses pipelines (`|`, `|&`, and the `!` status negation), the
`;`/`&&`/`||`/`&` connectors, and `NAME=VALUE` prefix assignments, expands
`$`-variables, runs a small set of builtins in-process, and launches
everything else through an injected process host with job control over
background and stopped jobs.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependencies are the audited `lib/*` crates `rustos-abi` (the stable
`Errno` carried back by the process-host seam), `rustos-resref` (the one
resource-reference spelling parser), and `rustos-vt` (the shared read line
discipline the REPL's line reader runs) — so a userland program never links
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

`Shell::run_line` is the one-shot entry point (it composes `Shell::parse_line`
and `Shell::run_list`, which callers that must collect here-document bodies —
the REPL — drive separately). For each line it:

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

Any line-aborting error — at parse *or* run stage (a failed expansion, a
missing here-document body) — is reported on standard error and sets `$?`
to `2` through one shared path, so nothing (further) from the line runs.

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
- **Pipe-both:** `a |& b` pipes both stdout and stderr; it is lowered once,
  in the parser, to its POSIX meaning — a `2>&1` duplication appended to the
  left-hand command — so the interpreter and host never re-derive it
  (`AGENTS.md` §2.2).
- **Multios (zsh):** repeating an output redirection for one descriptor fans
  the stream out to every target (`cmd >a >>b`), and repeating an input
  redirection reads the targets in order (`cmd <part1 <part2`). The
  interpreter merges the repeated opens into a single `Multi` action whose
  targets keep their own modes and are classified independently (a list may
  mix paths and resource references, `cmd >log >sys:null`). The host must
  open every target or apply nothing; a descriptor that mixes reading and
  writing opens (or the bidirectional `<>`) fails the line closed.
- **Dynamic descriptors (zsh):** `{var}>out` (and any other `<`/`>` operator
  with a `{name}` glued to it) allocates a fresh descriptor — always ≥ 10,
  never the reserved standard streams fd 0–3 — performs the redirection on
  it, and binds the number to the shell parameter `var`; `{var}>&-` closes
  the previously allocated descriptor read back from `$var`. A variable that
  does not hold an allocated number fails closed (the shell never closes a
  standard stream off a stale or mistyped value).
- **Here-document:** `<< delim` (and `<<- delim`, which strips leading tabs
  from body and terminator lines) feeds the following input lines, up to a
  line holding only the delimiter, as the input of its descriptor (default
  fd 0). The command line names only the delimiter; the body is collected
  afterwards (`CommandList::feed_here_doc_line`, driven by the REPL with a
  `> ` continuation prompt), in source order when a line has several
  here-documents. If any part of the delimiter was quoted the body is
  literal; otherwise it undergoes the same `$` expansion as a word. A
  complete body lowers to the same `HereString` bytes-on-fd action as a
  here-string — one primitive, not two (`AGENTS.md` §2.2). Collection is
  bounded (`MAX_HERE_DOC_BYTES`, 64 KiB — a fixed security bound, §24.4):
  an over-large body, or one that lost a line to the reader's line-length
  limit, is discarded and fails the line closed, but is still consumed to
  its terminator so the remaining body lines are never misread as commands.

A descriptor number is an IO number only when it is glued directly to a `<`/`>`
(so `echo 2` is a plain argument, but `2>err` names fd 2). The parser
**fails closed** (`AGENTS.md` §5.4, §2.9): a file redirection with no target,
an ambiguous duplication (`<&file`, `2>&file`), a here-string or here-document
with no target/delimiter word, an unterminated here-document (input ended
before its delimiter line), or an over-length here-document body runs
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
rather than misbehaving): process substitution — the stream forms `<(…)` /
`>(…)` await the launch plumbing, and the temporary-file form `=(…)` is
permanently unsupported (RustOS has no scratch filesystem) — and the compound
commands `( list )` and `{ list; }`. Each is *recognised* and aborts the line
with a parse error, so a parenthesised command can never be misread as a
filename and `{`/`(` can never run as a program name. Classifying a
target is done; *resolving* a `Resource` target to a kernel stream backing
(opening `sys:null`, the
capability-checked resolve of any other namespace) waits on the same launch
ABI that gates applying a file redirection. The `spawn` ABI carries the
child's argument vector and environment but no descriptor plumbing yet, so
the runtime process host reports `NotImplemented` for any redirection — a
`Path` and a `Resource` target alike — until the launch ABI grows pipes and
redirections; the parsing, classification, lowering, and fail-closed
semantics above are exercised in full against the in-memory fixtures.

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
  `rustos_rt::stdin`, reassembling lines across reads with the read line
  discipline's shared **buffer** half (`rustos_vt::line::LineEditor`,
  the same editor login's prompt reads run and the kernel console echo
  mirrors): CR and LF both terminate a line (a serial terminal sends CR for
  the Return key, a pipe or script LF, and a CRLF pair counts once), and the
  erase control (Backspace / Delete) rubs out the last kept byte. A line
  whose here-documents are pending is completed by reading body lines under
  a `> ` continuation prompt before anything runs.
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

## Interactive line editor and tab completion

When the input backing accepts the **raw** read discipline
(`stream_input_mode`), the REPL runs the interactive line editor
(`src/editor.rs`) instead of the plain reader: the shell echoes and repaints
the line itself through the shared `lib/vt` escape vocabulary, and decodes
key bytes through the shared `rustos_curses::Input` decoder — never a
shell-private key table (`plans/SHELL.md` "Interactive terminal"). A backing
that refuses raw mode (a pipe, a script) keeps the plain reader, so scripted
execution is byte-identical with or without a terminal. Around every launched
command the loop restores the cooked discipline and takes raw back at the
next prompt; leaving the session hands the console back cooked.

The editing set matches bash/zsh muscle memory: Up/Down (`Ctrl-P`/`Ctrl-N`)
walk the bounded in-memory history (draft preserved, blanks and consecutive
duplicates skipped); `Ctrl-R` is incremental reverse search (`Ctrl-R` steps
older, `Ctrl-G` aborts, Escape accepts, Enter accepts and runs);
Left/Right/Home/End (`Ctrl-A`/`E`/`B`/`F`) and `Alt-B`/`Alt-F` move; the
kill/yank set is `Ctrl-K`/`Ctrl-U`/`Ctrl-W`/`Alt-D`/`Ctrl-Y` with `Ctrl-T`
transpose; `Ctrl-C` cancels the line under edit (acknowledged as `^C`, `$?`
untouched); `Ctrl-D` deletes under the cursor or, on an empty line, ends the
session; `Ctrl-L` clears and repaints; bracketed paste inserts literally and
never auto-runs. Delivering `Ctrl-C`/`Ctrl-Z` to a *running foreground job*
is the staged kernel work in `.junie/plan-session-shell.md` (part 3).

Tab completion (`src/complete.rs`) locates the word under the cursor with
the shell's own span-carrying lexer (`lexer::tokenize_with_spans`) and is
read-only: it never runs a command, writes a file, or changes `$?`. A
command-position word completes from the builtin table and the `.app`
bundles of the shared command-search directories
(`rustos_cmdres::command_search_dirs` — so exactly the names the shell would
resolve are offered); argument words complete as filesystem paths through
the injected `WordLister` (kernel-authorised `fs_readdir`); a redirection
target additionally offers the registered resource namespaces (`sys:` …) and
their well-known selectors (`rustos_resref::KnownNamespace`) — the same
registry the redirection classifier applies, cross-checked against the
kernel resolver. A unique candidate inserts (directories stay open with `/`,
finished words close with a space), several extend to their longest common
prefix, and an unextendable set is listed inline. Unlexable prefixes and
quoted/expansion-bearing words degrade to no candidates, fail closed.

The `RtProcessHost` launches external commands through the `spawn` syscall
and reaps them through `wait`. The command's words travel to the child as
its argument vector and the shell's exported variables (with any `NAME=v
cmd` prefix overrides layered on top) as its environment, encoded into the
`spawn` startup-strings block — strings are data, never authority, and the
kernel re-validates the block fail-closed before building the child's own
copy. Pipes and redirections need descriptor plumbing the ABI does not yet
express, so the host fails a pipeline or redirection closed with
`NotImplemented` rather than silently dropping it. The in-process builtins
(`echo`, `exit`, `export`, `pwd`, `help`, …) work regardless.

## Builtins

`cd`, `pwd`, `exit`, `export`, `unset`, `echo`, `jobs`, `fg`, `bg`,
`ulimit`, `elevate`, `help`. A builtin runs inside the shell process
because it mutates or reads shell-side state — the environment, the
working directory, the job table, the `exit` request a read-eval loop
watches, the process's own resource limits, or (for `elevate`) the
controlling terminal a password prompt must own; everything else is
launched as an external program.

`elevate <user> <program>` is the per-invocation elevation frontend
(`plans/CAPABILITY_USE.md` CU5): it prompts for the target account's
password with echo suppressed, posts one synchronous IPC call to this
console's login supervisor over the reserved per-console rendezvous
(derived from the shell's **own** kernel-attested console, never a
claim), and blocks — a foreground elevated command — until the
re-authenticated program has run as that account; its exit code becomes
`$?`. The shell holds no elevation authority: authentication, placement
checking, and the identity switch all happen in the supervisor and the
kernel, the password buffer is zeroed on every path, and a shell with no
console-backed streams (a pipe, a network session) has no rendezvous and
fails closed. The seam is `Elevator` (`host.rs`), fail-closed by default
and backed in the `Run` binary by `self_origin` + `ipc_call`.

A builtin may carry `NAME=VALUE` prefix assignments; they bind for the
builtin's duration only and are restored afterwards (the command's
environment, not the shell's). A *redirection* on a builtin fails closed
with status 1: builtins write through the injected console seam, and
silently sending a redirected stream to the terminal would be worse than
refusing (`AGENTS.md` §5.4).

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
- The supported expansions are `$NAME`, `${NAME}`, and `$?`. Command
  substitution, arithmetic expansion, brace expansion, tilde expansion, and
  filename generation are not implemented yet; their spellings are inert
  word text except where running them would change command meaning — process
  substitution and compound commands fail closed with a parse error.

## Tests

`cargo test -p rustos-elsh` drives the interpreter against in-memory
`Console`/`ProcessHost` fixtures, covering the lexer's quoting and escape
rules, the parser's pipelines/redirections/connectors and its fail-closed
grammar errors, `$`-expansion, here-document collection (quoted/unquoted
delimiters, `<<-` tab stripping, source-order filling, the size bound, and
the unterminated/over-length fail-closed paths, including through the REPL),
every builtin, foreground status propagation, the command-not-found path,
background job tracking, the `Done`-before-prompt reporting of finished
jobs, connector short-circuiting, `!` status negation, `|&` lowering,
prefix-assignment scoping (child environment only; temporary around a
builtin), multios fan-out/concatenation and its fail-closed mixed-direction
case, `{var}` dynamic-descriptor allocation/close, and the fail-closed
process-substitution and compound-command spellings.
