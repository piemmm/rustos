# IO.md — First-party Rust I/O abstraction over the descriptor table

This is a staged build plan for RustOS's userland I/O **library** layer. It is
**binding under `AGENTS.md`**; read `AGENTS.md` and `PLAN.md` first. Every rule
in both applies here without exception. This plan exists because the charter
requires a `lib/*` crate proposal to be written and approved in a plan file
*before* any API is invented (`AGENTS.md` §6, §15.2, §2.2 — one I/O
vocabulary, no duplication).

**Note:** `abi-v1` is *not* frozen, despite what `AGENTS.md` / `PLAN.md` say —
the standing task direction supersedes that language. This plan, however, adds
**no** ABI surface of its own: it is a pure-Rust convenience layer over the
existing `abi-v1` `stream_read` / `stream_write` traps, so `lib/abi`, the
syscall table, and the C header are untouched by it. The descriptor-*producing*
ABI it consumes (opening a file or a resource reference to a new fd) is owned by
its sibling plans, not invented here (§4, §5).

## 0. Why this exists

Today every text program (the shell, `init`, `sysinfo`, the CLI utilities,
system services) does I/O by calling the thin `lib/rt` syscall wrappers
directly — `rustos_rt::stdout(bytes)`, `rustos_rt::stderr(bytes)`,
`rustos_rt::stdinfo(bytes)`, `rustos_rt::stdin(&mut buf)` — passing raw byte
slices over the `abi-v1` `stream_write` / `stream_read` traps (`AGENTS.md`
§20). That floor is correct and stays: §20 already forbids reaching for a
console/UART/framebuffer device, and software must keep doing I/O over
inherited fd 0/1/2/3 only.

What is **missing** is the ergonomic *library* on top of those wrappers — the
RustOS equivalent of the `std::io` surface that real shells and tools program
against instead of hand-marshalling byte slices and re-looping every short
read/write themselves:

- a `Read` / `Write` trait layer (over a stream descriptor, never over a
  device);
- buffered readers/writers so a tool is not one syscall per byte;
- line reading for the REPL (`stdin` → lines);
- `write!` / `writeln!`-style formatting into a stream without a heap
  allocation per call.

Without this each program re-implements the same short-write loop and the same
"read until newline" logic — exactly the duplication `AGENTS.md` §2.2 forbids.

A second, equally important reason this plan exists is **groundwork**. The
standard-stream floor (§20) is the *first* user of the descriptor table, not
the only one. Filesystem reads/writes (`plans/DRIVES.md`), resource-reference
streams such as `sys:null` / `tty:` / `disk:` (`plans/ALIAS.md`,
`plans/SHELL.md`), USB-storage-backed files, serial/tty backings, and pipes are
all *future* fd backings the kernel resolves the same descriptors against. If
this layer is bound to "the four standard streams" it will force a **second**
I/O vocabulary the day files or devices land — the §2.2 defect. So this layer
is designed **fd-generic from IO1**: one `Read`/`Write` definition that the
standard streams, opened files, and opened resource references all reuse. This
plan does not *create* those other backings; it makes sure the byte-movement
vocabulary they share already exists and has exactly one definition.

## 0a. Status — what already exists vs. what this plan adds

The descriptor-table **floor** this layer sits on is **already implemented**
(`PLAN.md` Stage 6); this plan adds only the Rust library on top. Marked so a
future reader does not re-do landed work (`AGENTS.md` task direction — mark
done items):

- **DONE — `abi-v1` standard-stream syscalls.** `stream_write(fd, buf, len)`
  and `stream_read(fd, buf, len)` exist in `lib/abi`'s syscall table and
  **already take an arbitrary `fd`** resolved against the per-process
  descriptor table (`kernel/syscall`). They are *not* hard-wired to fd 0/1/2/3
  — the wrappers are. Consequence: a fd-generic `Read`/`Write` needs **no new
  syscall** (see §4).
- **DONE — per-process descriptor table + per-console sessions.** Each
  descriptor records its kernel stream backing; the spawner establishes a
  child's fd 0/1/2/3 at spawn time (`spawn`'s console selector,
  `console_count`), and the read line discipline (echo, CR/LF) is the kernel's
  (`stream_echo`). Current backings are the discovered text consoles
  (video + UART) only.
- **DONE — `lib/rt` thin wrappers.** `rustos_rt::{stdin, stdout, stderr,
  stdinfo, set_echo}` marshal byte slices over the traps. `lib/rt` registers
  the process heap (`AGENTS.md` §25), so `alloc` is available to this layer.
- **DONE — `stdinfo` framing.** The `StdInfoRecord` JSONL model lives in
  `lib/abi` (`stdinfo.rs`, `AGENTS.md` §20.1). This layer carries the bytes; it
  does not redefine the record.
- **DONE — the library (IO1–IO3).** The `Read`/`Write` trait layer, the
  fd-generic non-owning `Stream`, the four standard streams, buffering
  (`BufReader`/`BufWriter`, `read_line`/`read_until`/`lines`), and formatting
  (`write_fmt`) live in `lib/rt/src/io.rs` (module `rustos_rt::io`), with host
  unit tests and rustdoc + `docs/src/lib/rt-io.md`.
- **DONE — userland adoption (IO4).** The in-tree callers that hand-rolled a
  short-write loop over the `lib/rt` byte-slice wrappers now write through
  `rustos_rt::io::{Stdout, Stderr, Write}`, and the duplicated loops are
  deleted: `userland/shell/elsh` (`RtConsole`), `userland/session/login`
  (`RtPrompt`), `userland/apps/top` (`RtTty`), `userland/system/init` (the
  banner write), and `lib/procinfo` (`RtOutput` / `write_stderr_line`, which
  back `sysinfo` / `ps` / `top`). There is one `Write::write_all` loop in
  userland. The bounded, edit-aware line readers that are **not** the unbounded
  `BufReader` — the shell REPL's `MAX_LINE`-capped `LineReader` and login's
  `push_line_byte` editor — are a deliberate security bound, not the
  duplication IO4 removes, so they stay as their own readers over the `read`
  primitive.
- **DECIDED — no owning/close-on-drop handle yet.** IO1 deliberately ships a
  *non-owning* `Stream` (a view of an fd the process already owns), not an
  owning RAII closer: `abi-v1` has no generic descriptor-close trap (only the
  filesystem's `File`, which closes via `fs_close`), so a close-on-drop handle
  would be a speculative interface bound to a syscall that does not exist
  (`AGENTS.md` §2.4). It lands with the descriptor-producing/closing ABI that
  will own it (`plans/DRIVES.md` / `plans/ALIAS.md`).
- **NOT STARTED, OWNED ELSEWHERE — the descriptor-*producing* ABI.** The
  syscall(s) that resolve a file path or a resource reference to a *new* fd, and
  the closed `sys:` stream-backing enum, are unimplemented and are owned by
  `plans/DRIVES.md` (files), `plans/ALIAS.md` + `plans/SHELL.md` (resource
  references). This plan depends on them but must not invent them (§5).

## 1. Scope and decisions (binding for this plan)

- **One I/O vocabulary, rolled first-party** (`AGENTS.md` §2.2, §2.12): a
  single `Read`/`Write` trait pair plus the buffering and formatting built on
  them, defined **once** and reused by every fd backing — the standard streams
  today, opened files / resource references / tty / pipes tomorrow. No second
  I/O abstraction is introduced anywhere; the existing `lib/rt` free functions
  become the *backing* the standard-stream handles call, not a parallel surface
  (see §4).
- **fd-generic, not stream-specific** (the groundwork, `AGENTS.md` §2.2): the
  trait layer operates on an **owned stream descriptor** (a thin, safe wrapper
  over a raw `fd`), not on four hard-coded singletons. `Stdin`/`Stdout`/
  `Stderr`/`StdInfo` are the *well-known* descriptors (fd 0/1/2/3); any fd a
  sibling plan later opens (a file, a `sys:`/`tty:`/`disk:` backing, a pipe end)
  is read/written through the **same** traits. This is what guarantees there is
  never a second I/O vocabulary when storage, USB, or serial land.
- **Strictly a layer over `abi-v1` — no new authority** (`AGENTS.md` §5.4):
  every trait method ultimately calls `stream_read(fd, …)` / `stream_write(fd,
  …)`, which the kernel resolves against the caller's descriptor table and
  capability set. This crate adds **no** syscall, no capability, and no
  `lib/abi` type. A program reaches no I/O it could not already reach: holding
  an `OwnedStream` is holding an fd the kernel already gave the process, never
  ambient authority to open a new one.
- **Open is not this layer's job** (`AGENTS.md` §2.4, §17.4): obtaining a *new*
  fd — opening a file under a capability/`FileCap` (`plans/DRIVES.md`),
  resolving a resource reference with a `Read`/`Write` resolve intent to a
  stream backing (`plans/ALIAS.md` §15.3, `plans/SHELL.md`) — is a
  capability-bearing operation owned by those plans. This layer **consumes** the
  resulting fd; it exposes no `open()`/`resolve()` and so cannot be used to
  widen authority (§5).
- **Bind to descriptors, never a device** (`AGENTS.md` §20): the only concrete
  I/O objects this layer constructs on its own are the four inherited standard
  streams. It exposes **no** console, UART, or framebuffer object and never
  calls a device syscall (`console_read`/`console_write` are a kernel-internal
  *backing* the stream layer attaches to fd 0/1 during boot, never a
  program-facing surface, §20). An `OwnedStream` for any other fd is only ever
  *handed in* by the owning plan's open/resolve call — this layer never
  fabricates one.
- **`fd 3` (`stdinfo`) keeps its §20.1 semantics**: best-effort, non-blocking,
  ignorable, never affecting correctness. The `Write` impl for the stdinfo
  stream must not let a full/absent consumer turn into an error a program
  depends on; structured `StdInfoRecord` framing stays in `lib/abi`
  (`AGENTS.md` §20.1) — this layer only carries the bytes.
- **Logging is not I/O over this layer** (`AGENTS.md` §19.4, §20.1;
  `plans/SYSLOG.md`): security/audit and structured log *records* flow through
  `lib/log` (a logging syscall / IPC / trusted runtime path that attaches
  system-attested origin), **never** as bytes written through these traits. A
  CLI tool that *displays* logs (`log show`, `log find`, …) writes its rendered
  text to `stdout`/`stderr`/`stdinfo` through this layer like any other program,
  but the authoritative log path is `lib/log`, not this crate. This layer must
  expose no "log to a stream" shortcut that would become a second, unattested
  log path (§2.2, §2.4).
- **Fail closed, no panics** (`AGENTS.md` §2.9, §5.4): a short read/write is a
  value the API loops over, EOF is represented honestly, and no path uses
  `unwrap`/`expect`/`panic!`. A formatting error surfaces as an `Err`, not a
  panic.
- **`no_std` + `alloc`** (`AGENTS.md` §6): the crate is `no_std`. Buffers are
  fixed-capacity where an allocation-free path is required (the kernel-adjacent
  and early-boot callers); any `alloc`-using convenience is gated so the
  allocation-free core is always available.
- **No C `stdio`** (`AGENTS.md` §16.4, `plans/CCOMPAT.md`): RustOS does **not**
  ship a system-wide C `stdio` (`FILE*`/`fopen`/`fwrite`/`printf`). The
  *System runtime / C ABI* class stays deliberately minimal — the
  `ros_sys_stream_*` stubs + `crt0` only. A third-party C program brings its
  own libc/`stdio` inside its app bundle (`AGENTS.md` §16.4, §16.5). Building a
  RustOS-maintained C `stdio` would be forbidden curated-library bloat
  (§2.3) and is explicitly out of scope here.
- **No stubs** (`AGENTS.md` §15.1): each stage ships code **plus** tests
  **plus** docs, and is only "done" when the whole-project gate (§7) is green.

## 2. Where it lives and layering (one-way edges, `AGENTS.md` §17.4)

The trait layer and buffering belong with the userland runtime a Rust program
already links. Decision (to be confirmed in IO1):

```
lib/rt  → lib/abi, lib/abi-trap (existing thin syscall wrappers)   [backing]
lib/rt::io  (a module, not a new crate)                            [Read/Write + buffering + fmt]
userland (shells/apps/services) → rustos_rt::io                    [the I/O surface]
```

Rationale for a **module inside `lib/rt`** rather than a new `lib/io` crate:
the trait layer is meaningless without the runtime's stream wrappers it sits
on, it adds no authority, and a one-purpose sibling crate that only re-exports
`lib/rt` would be the bloat `AGENTS.md` §2.3/§15.5 forbids. The fd-generic
design (§1) does **not** by itself justify a separate crate: an `OwnedStream`
is still just an fd the runtime owns, and the open/resolve calls that mint new
fds live in their own plans' crates, not here. If IO1 finds a concrete second
consumer that must use the I/O traits *without* `lib/rt`'s `_start`/panic
machinery (for example a `lib/*` crate consumed by both userland and a
non-`rt` context), a `lib/io` crate is justified instead and §3/§17.4 of
`AGENTS.md` are updated in the same change. Either way there is exactly one
`Read`/`Write` definition.

This layer is **not** a curated `/System/Libraries/` class: it is internal
runtime plumbing linked into a program (like the rest of `lib/rt`), not a
dynamically-linked OS library, so §16.4 is unchanged. (Contrast `lib/curses`,
which *is* a curated class — see `plans/CURSES.md`.)

## 3. Staged increments

Each stage is one fully-gated landing (`AGENTS.md` §7 / §2.15): code + tests +
rustdoc + the relevant `docs/` page, whole-project gate green.

- **IO1 — `Read`/`Write` traits + the fd-generic stream handle + the four
  standard streams. DONE.** The `Read`/`Write` traits handle the
  short-read/short-write loops inside `read_exact` / `write_all`; the
  `Error`/`Result` fails closed; the fd-generic **non-owning** `Stream` (see
  the DECIDED note above — not an owning `OwnedStream`, pending a
  descriptor-close trap) carries the shared read/write path; and the zero-cost
  `Stdin`/`Stdout`/`Stderr`/`StdInfo` accessors delegate to the crate-private
  `stream_read` / `stream_write` primitives (so `stdin` and the fd-generic
  reader share one definition). `StdInfo`'s `Write` honours §20.1 (best-effort,
  never a short write or error on no consumer). Placement decision confirmed: a
  module inside `lib/rt` (`rustos_rt::io`), not a new crate. Tests cover the
  short-write loop reaching full length, `read_exact`/EOF semantics, `stdinfo`
  never stalling `write_all`, and a `Stream` over a non-standard fd taking the
  identical trap path as `Stdout` (proves §2.2). Docs: `docs/src/lib/rt-io.md`
  + the crate `README.md`.
- **IO2 — buffering. DONE.** `BufWriter` (fixed-capacity inline array, coalesces
  small writes, explicit `flush`, best-effort flush-on-drop, oversized write
  passes through untorn) and `BufReader` (`read_until`/`read_line`/`lines` for
  the REPL). Buffer capacity is a const generic (`CAP`), so the buffer itself
  needs no heap allocation. Tests cover the overflow-flush boundary, partial
  line accumulation across short reads, terminator stripping, and invalid-UTF-8
  rejection.
- **IO3 — formatting. DONE.** `write!` / `writeln!` via a `write_fmt` provided
  method that renders through a `core::fmt::Write` adapter capturing the first
  I/O error, so a `fmt::Error` surfaces as `Error::Fmt`, never a panic. Tests
  cover formatted output bytes and the error path.
- **IO4 — adopt across userland (delete the hand-rolled loops). DONE.** The
  in-tree callers (`userland/shell/elsh`, `userland/system/init`,
  `userland/apps/top`, and the `sysinfo` / `ps` / `top` output path shared
  through `lib/procinfo`) write through `rustos_rt::io::{Stdout, Stderr,
  Write}`, and the open-coded short-write loops they carried are **deleted**
  (no dead code, no parallel I/O paths). After IO4 there is one
  `Write::write_all` loop in userland — this is the stage that proves §2.2.
  The bounded/edit-aware line readers (the REPL's `MAX_LINE` `LineReader`,
  login's `push_line_byte` editor) are retained: they are a security bound
  (§24.4), not the unbounded `BufReader`, so collapsing them would *lose* a
  bound rather than remove duplication. Verified by the existing
  shell/init/utility host tests and the `spawn_session_qemu_aarch64` vertical
  (a child's fd-1 output).

## 4. How the siblings plug in (groundwork, no IO.md ABI)

This layer is the *consumer* end of one fd-generic vocabulary; the sibling
plans own the *producer* end (the open/resolve calls that mint new fds and the
kernel stream backings behind them). Recorded here so each future feature has a
clear, roadblock-free path and **does not** grow a second I/O surface. None of
the producer ABI is invented in this plan.

- **Filesystem reads/writes (`plans/DRIVES.md`).** Opening a path resolves to a
  capability-checked file/handle (`root_handle` + `directory_handle`,
  `FileCap`, delegated handles — never a bare pathname, `AGENTS.md` §4, §5.3).
  That open call yields an fd; this layer's `Read`/`Write` move the bytes. The
  block/USB-storage → filesystem-driver → file-handle → fd chain is owned by
  the storage/filesystem plans; the **only** requirement this plan places on
  them is that the readable/writable end is an fd resolved against the
  descriptor table, so no separate "file I/O" trait is needed.
- **USB storage (no roadblock).** A USB mass-storage device is reached through
  the normal driver path (`drivers/bus/usb/*`, `drivers/storage/*`,
  `plans/USB.md`) and surfaces as a block device the filesystem driver mounts;
  files on it open to fds exactly as above. The driver's own device I/O (MMIO
  doorbells, DMA rings, `dma-barrier`) is a *different* layer (`lib/drvrt`,
  device-resource grants) and deliberately does **not** use these standard-
  stream traits — keeping that boundary explicit is part of the groundwork.
- **Serial / tty and the `sys:` byte streams (`plans/ALIAS.md`,
  `plans/SHELL.md`).** A redirection or command resolves a resource reference
  (`tty:debug`, `sys:null`, `sys:zero`, `sys:full`, `sys:random`, `disk:`) with
  a `Read`/`Write` resolve intent to a **closed, versioned, hashed `lib/abi`
  stream-backing enum** (owned by `ALIAS.md`/`SHELL.md`, not a string-keyed
  device table). The resolver hands back an fd; this layer reads/writes it. A
  serial console used interactively is just another fd backing — the shell
  still binds only fd 0/1/2/3 and never a UART device (`AGENTS.md` §20).
  `sys:random` draws from the one kernel CSPRNG (`AGENTS.md` §22), never a
  second entropy path.
- **Logging (`plans/SYSLOG.md`, `AGENTS.md` §19.4).** Structured/audited log
  *records* never travel through these traits; they go through `lib/log`. The
  `log` CLI tools render text to `stdout`/`stderr`/`stdinfo` through this layer,
  but this layer offers no record-to-stream path that would bypass the
  attested journal ingress (§1).
- **Pipes and redirection (`plans/SHELL.md`).** `cmd | next`, `cmd >file`,
  `cmd 3>info.jsonl`, `cmd 3>&-` all work by the spawner wiring the child's fd
  0/1/2/3 to the appropriate stream backings *before* exec; the child still
  only names descriptors. The fd-generic traits make a piped/redirected fd
  indistinguishable from an inherited standard stream to the program — which is
  exactly the device independence §20 requires.

## 5. Non-negotiable invariants (recap)

- Adds no `abi-v1` surface, no syscall, no capability — a pure layer over the
  existing `lib/rt` stream wrappers and the existing fd-taking `stream_read` /
  `stream_write` traps (`AGENTS.md` §5.4).
- One fd-generic `Read`/`Write` definition for the whole OS userland
  (`AGENTS.md` §2.2): the four standard streams today, and every file / resource
  / tty / pipe fd a sibling plan later opens, share it; IO4 deletes the
  duplicated loops it supersedes (`AGENTS.md` §2.14).
- Exposes only the four inherited standard streams as objects it constructs;
  any other fd is *handed in* by the owning plan's capability-checked
  open/resolve call. No device object, no `open()`/`resolve()` here, no ambient
  authority (`AGENTS.md` §20, §4, §2.4).
- Log records go through `lib/log`, never these traits (`AGENTS.md` §19.4,
  §20.1; `plans/SYSLOG.md`).
- `no_std`, fail-closed, no `unwrap`/`expect`/`panic!` in production paths
  (`AGENTS.md` §2.9), no stubs (§15.1), tests + docs in the same change
  (§7, §13).
- No RustOS-maintained C `stdio`; the C ABI class stays minimal
  (`AGENTS.md` §16.4, `plans/CCOMPAT.md`).
