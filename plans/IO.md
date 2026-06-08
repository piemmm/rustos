# IO.md — First-party Rust I/O abstraction over the standard streams

This is a staged build plan for RustOS's userland I/O **library** layer. It is
**binding under `AGENTS.md`**; read `AGENTS.md` and `PLAN.md` first. Every rule
in both applies here without exception. This plan exists because the charter
requires a `lib/*` crate proposal to be written and approved in a plan file
*before* any API is invented (`AGENTS.md` §6, §15.2, §2.2 — one I/O
vocabulary, no duplication).

**Note:** `abi-v1` is *not* frozen, despite what `AGENTS.md` / `PLAN.md` say —
the standing task direction supersedes that language. This plan, however, adds
**no** ABI surface: it is a pure-Rust convenience layer over the existing
`abi-v1` standard-stream syscalls, so `lib/abi`, the syscall table, and the C
header are untouched by it.

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

- a `Read` / `Write` trait layer (over the §20 streams, never over a device);
- buffered readers/writers so a tool is not one syscall per byte;
- line reading for the REPL (`stdin` → lines);
- `write!` / `writeln!`-style formatting into a stream without a heap
  allocation per call.

Without this each program re-implements the same short-write loop and the same
"read until newline" logic — exactly the duplication `AGENTS.md` §2.2 forbids.

## 1. Scope and decisions (binding for this plan)

- **One I/O vocabulary, rolled first-party** (`AGENTS.md` §2.2, §2.12): a
  single `Read`/`Write` trait pair plus the buffering and formatting built on
  them. No second I/O abstraction is introduced anywhere; the existing
  `lib/rt` free functions become the *backing* the trait impls call, not a
  parallel surface (see §4).
- **Strictly a layer over `abi-v1` — no new authority** (`AGENTS.md` §5.4):
  every trait method ultimately calls an existing `lib/rt` wrapper
  (`stdout`/`stderr`/`stdinfo`/`stdin`), which traps to an existing syscall.
  This crate adds **no** syscall, no capability, and no `lib/abi` type. A
  program reaches no I/O it could not already reach.
- **Bind to the standard streams, never a device** (`AGENTS.md` §20): the only
  concrete I/O objects this layer exposes are the four inherited standard
  streams (`stdin`/`stdout`/`stderr`/`stdinfo`). It exposes **no** console,
  UART, or framebuffer object, so it cannot be used to bypass §20.
- **`fd 3` (`stdinfo`) keeps its §20.1 semantics**: best-effort, non-blocking,
  ignorable, never affecting correctness. The `Write` impl for the stdinfo
  stream must not let a full/absent consumer turn into an error a program
  depends on; structured `StdInfoRecord` framing stays in `lib/abi`
  (`AGENTS.md` §20.1) — this crate only carries the bytes.
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
`lib/rt` would be the bloat `AGENTS.md` §2.3/§15.5 forbids. If IO1 finds a
concrete second consumer that must use the I/O traits *without* `lib/rt`'s
`_start`/panic machinery, a `lib/io` crate is justified instead and §3/§17.4
of `AGENTS.md` are updated in the same change. Either way there is exactly one
`Read`/`Write` definition.

This layer is **not** a curated `/System/Libraries/` class: it is internal
runtime plumbing linked into a program (like the rest of `lib/rt`), not a
dynamically-linked OS library, so §16.4 is unchanged. (Contrast `lib/curses`,
which *is* a curated class — see `plans/CURSES.md`.)

## 3. Staged increments

Each stage is one fully-gated landing (`AGENTS.md` §7 / §2.15): code + tests +
rustdoc + the relevant `docs/` page, whole-project gate green.

- **IO1 — `Read`/`Write` traits + the four standard streams.**
  Define the `Read` and `Write` traits (short-read/short-write loops handled
  *inside* `write_all` / `read` helpers so callers stop re-implementing them),
  an `Error`/`Result` that fails closed, and the concrete zero-sized stream
  handles `Stdin`/`Stdout`/`Stderr`/`StdInfo` whose impls call the existing
  `lib/rt` wrappers. `stdinfo`'s `Write` honours §20.1 (best-effort). Confirm
  the §2 placement decision and record it in `AGENTS.md` §3 if a new crate is
  chosen. Tests: short-write loop reaches full length, EOF/`read` semantics,
  stdinfo never errors on no-consumer. Docs: `docs/src/lib/rt-io.md` (or
  `docs/src/lib/io.md`) + the crate `README.md` stability tier (`AGENTS.md`
  §6).
- **IO2 — buffering.** `BufWriter` (coalesces small writes, explicit `flush`,
  flush-on-drop best-effort) and `BufReader` with `read_line` / `lines` for the
  REPL. Fixed-capacity buffers for the allocation-free path. Tests: buffer
  fills/flushes at the boundary, partial-line reads accumulate, a write
  spanning the buffer boundary is not torn.
- **IO3 — formatting.** `write!` / `writeln!` support by implementing
  `core::fmt::Write` on the buffered writer (and a `format_args!`-based helper),
  so a tool emits formatted output without a per-call heap allocation. Tests:
  formatted output matches expected bytes; a formatting `fmt::Error` surfaces
  as the crate `Err`, never a panic.
- **IO4 — adopt across userland (delete the hand-rolled loops).** Migrate the
  in-tree callers (`userland/shell/shell`, `userland/system/init`,
  `userland/apps/*`, `sysinfo`, services) to the new surface and **delete** the
  open-coded short-write loops and ad-hoc line buffers they replace
  (`AGENTS.md` §2.14 — no dead code, no parallel I/O paths). This is the stage
  that proves §2.2: after IO4 there is one I/O vocabulary in userland. Tests:
  the existing shell/init/utility tests still pass against the new surface; the
  `spawn_session_qemu_aarch64` vertical still proves a child's fd-1 output.

## 4. Non-negotiable invariants (recap)

- Adds no `abi-v1` surface, no syscall, no capability — pure layer over the
  existing `lib/rt` stream wrappers (`AGENTS.md` §5.4).
- Exposes only the four inherited standard streams; no device object
  (`AGENTS.md` §20).
- One `Read`/`Write` definition for the whole OS userland (`AGENTS.md` §2.2);
  IO4 deletes the duplicated loops it supersedes (`AGENTS.md` §2.14).
- `no_std`, fail-closed, no `unwrap`/`expect`/`panic!` in production paths
  (`AGENTS.md` §2.9), no stubs (§15.1), tests + docs in the same change
  (§7, §13).
- No RustOS-maintained C `stdio`; the C ABI class stays minimal
  (`AGENTS.md` §16.4, `plans/CCOMPAT.md`).
