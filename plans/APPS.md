# APPS — Application structure, command help, and command resolution

This document is the normative specification for how a RustOS application is
**structured on disk**, how its **command-line help is authored and served**,
and how the shell (`elsh`, `plans/SHELL.md`) **resolves a typed command name**
to a runnable application. It extends — it does not replace — the fixed `.app`
bundle contract in `AGENTS.md` §16.5.

`AGENTS.md` is binding and wins over this document wherever they disagree.
This spec also defers to its companions and MUST stay consistent with them:

- **Application bundles / ABI** — `AGENTS.md` §9, §16.4, §16.5 and
  `lib/abi/src/appinfo.rs` (`BundleEntry`, `validate_bundle_layout`,
  `AppInfoHeader`) own the signed manifest, the fixed top-level layout, and the
  dynamic-loader library policy. `userland/system/appmgr` owns loading.
- **Shell** — `plans/SHELL.md` owns command parsing, builtins, job control,
  and the standard-stream model. This document adds command *resolution* and
  the `man`/`-h` help surface the shell exposes.
- **Paths / aliases** — `plans/DRIVES.md` (path spelling, `System:`/`Apps:`)
  and `plans/ALIAS.md` (resource references). No second path or reference
  parser is defined here (§2.2).
- **Terminal stack** — `plans/CURSES.md` (`lib/vt`, `lib/termcap`,
  `lib/curses`). Help rendering to a terminal goes through that one vocabulary.

## Terminology

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are implementation requirements.

- **App bundle** — a `<Name>.app` directory under an app store (§16.5).
- **Command app** — an app whose manifest permits command-line execution, so a
  user can run it by typing its command name at the shell.
- **System app store** — the OS-provided, read-only, system-signed set of
  command apps the shell searches first (see "Command resolution").
- **Help document** — one structured Markdown file describing one command
  (our modern replacement for a Unix man page).
- **Locale** — a BCP-47 language tag (e.g. `en-US`, `fr-FR`). `en-US` is
  the mandatory canonical locale every fallback chain ends at.

## Status

**In progress.** The maintainer decided the once-open design question in
favour of the **merge**: there is no separate `Documentation/` bundle entry —
the single internationalised `Help/` tree serves the CLI `man`, each
command's short `-h`/`-?` help, and any graphical help viewer (bundle-local
app documentation only; the OS source-tree docs under `docs/` are unrelated).
Landed: deliverable 1 (`BundleEntry::Help` replaced `Documentation` in place,
`AGENTS.md` §16.5 amended, C header regenerated), deliverable 2 (the
`lib/help` engine), deliverable 3 (the `man.app` command app, §7, with its
own thirteen-locale `Help/` tree shipped on the read-only `/System` volume and
the `LANG` locale variable named in §5), deliverable 4 (shell command
resolution over the `/System/Apps/` system app store, `AGENTS.md` §16.2
amended), and deliverable 6 for **every store-registered command app**:
`cat`, `ls`, `ps`, `top`, `sysinfo`, `users`, and `elsh` each ship a thirteen-locale
`Help/` tree in their bundle (discovered by `tools/syshelp` from the
`userland/apps` and `userland/shell` roots, planted by `tools/mkimage`
and the QEMU fixture) and honour the §4 `-h`/`-?` short-help convention
through the one shared `lib/help` render (`own_short_help` + the
`rt`-feature `BundleHelp` own-bundle source, which every `Run` binary
reuses instead of a private copy), deliverable 5 (`cargo xtask
help-lint`, the §8.1 gate wired into `cargo xtask ci`), and deliverable 8
increments 1–4 — self-contained on-disk bundles: every discovered
bundle's signed `AppInfo` + `Run` ships on disk beside its `Help/`, and
on the aarch64 production boot the `spawn` syscall loads and verifies
the on-disk store bundles through the shared `rustos_appload` gate (no
kernel-baked rxe rows remain there). Remaining: deliverable 7 (wider
`stdinfo` adoption — `man`'s locale-fallback, `ls`'s hidden-entries
omission, and the shared self-scope omission record `ps` and `sysinfo
processes` both emit are live; the other registered commands have
nothing non-obvious to add today), `Help/`
trees for future command apps as each becomes a registered bundle,
the §12.1 Stage B remainder (`chmod`/`chown`/`getcap`/`setcap`/`mount`
blocked on their kernel syscalls — `cp`/`mv`/`rm`/`useradd`/`groupadd`
are registered store bundles), the §12.1 Stage C remainder (the first
batch's `true`/`false`/`yes`/`basename`/`dirname`/`mkdir`/`rmdir`/
`head`/`wc`/`tee`/`seq`/`whoami` are registered store bundles; the rest
land in further batches), and
deliverable 8 increment 5 (the x86_64/riscv64 storage floor, then
deletion of the embedded registry those ports still carry as their
§18.6 boot floor). Help documents are authored **only** in
each bundle's
on-disk `Help/` tree and read at runtime through the `lib/help` seam — no app
embeds its help, and the image builder plants the trees from data discovered
by `tools/syshelp`, never a hand-maintained per-bundle list (§6.1, `AGENTS.md`
§16.5).

## Immediate work — kernel defects blocking further APPS stages

Four kernel defects observed on the running system MUST be fixed before the
remaining APPS deliverables continue. Each item records its root cause and
design (or landed behaviour) so any context can pick it up; all four are
now done. Statuses per `AGENTS.md` §13.

### I1. Idle load average pinned at ~1 — IPC wake-all thundering herd

**Status: done** (verify the idle load on a live single-core QEMU session
when convenient).

- **Defect:** at idle on a single-core QEMU instance the 1/5/15-minute load
  averages gravitated to 1, not ~0. Root cause: the `ipc_call` handler woke
  **every** parked IPC server (`serve_wake` → `SERVE_WAITQ.wake_all`) and
  `call_reply` woke **every** parked caller — a spuriously-woken server
  (e.g. the in-kernel driver-store kthread) was `Ready` at the instant the
  load census sampled the run queue. The `LoadTracker` maths, the observer
  exclusion, and every park path were verified correct; the herd was the
  sole cause (the `AGENTS.md` §27.3 wake-one defect).
- **Landed behaviour:** addressed IPC events wake exactly their target.
  `CallEndpoint` records the serving task's scheduler id at first receive
  (`record_server_task`, set by the `call_recv` handler and the in-kernel
  store serve loop) and each `post` captures the poster's scheduler id;
  `reply` returns it. `ipc_call` wakes only the recorded server
  (`waitq::serve_wake_task` over the new `WaitQueue::wake_task`) and every
  reply site wakes only the ticket's poster (`call_wake_task`), falling back
  to the broadcast only for an unrecorded server / id-less poster. Endpoint
  destruction keeps the broadcast so cancelled callers re-poll fail-closed.
  Covered by unit tests in `kernel/ipc/src/call.rs`
  (`reply_returns_the_posters_scheduler_id`,
  `server_task_is_recorded_once_and_zero_is_rejected`) and
  `kernel/core/src/waitq.rs`
  (`wake_task_unparks_only_the_named_registered_waiter`); the wake
  discipline is documented in `docs/src/architecture/scheduler.md`.

### I2. Process memory is never reclaimed on exit — login/logout RAM growth

**Status: done** (verify the stable login/logout RAM on a live QEMU session
when convenient), with one staged follow-up below.

- **Defect:** repeated login/logout grew used RAM monotonically: teardown
  reclaimed the kernel stack, IRQ bindings, endpoints, shared regions, and
  the capability record, but no user frames and no page-table frames.
- **Landed behaviour:** teardown is owned by the retained live space.
  `LiveSpace::drop` (`kernel/mem/src/live.rs`, run when the scheduler reap
  drops the control block's `Box<dyn LiveUserSpace>`) drains every live DMA
  carve, then releases every remaining tracked mapping — a page inside the
  MMIO/shared windows is only unmapped (device- or registry-owned frames);
  every other frame (image, user stack, startup block, anon heap) is zeroed
  through the physmap (zero-on-free, §4) and freed — and finally returns the
  page-table hierarchy post-order through the one shared
  `rustos_arch_api::frames::reclaim_hierarchy` walk each port's
  `mmu::AddressSpace::reclaim_table_frames` override drives into the new
  `PageTableFrames::free_table` (allocator-backed `FrameTableSource`
  recycles; the boot pools retire without reuse). SMP safety is an
  invariant, not luck: each port publishes a set-once **park root** (the
  permanent boot translation; aarch64/riscv64 in the boot `switch()`, x86_64
  from the trampoline `CR3` in `try_boot`), and the dispatcher re-parks a
  CPU off a user root at every task suspend
  (`KernelArch::park_translation` → `install_park_translation` →
  `dispatch_step`), so a dead root is never a CPU's active translation when
  its tables are freed (the port reclaim re-parks defensively and retires
  frames unreclaimed rather than dismantling an active regime — fail
  closed). A task that exits without a live space (kthreads) reclaims
  nothing, as before. Tests: `LiveSpace` whole-footprint drop test and the
  `spawn_image` spawn/exit-cycle test (`kernel/core/src/spawn.rs`) assert
  `free_frames` returns exactly to its pre-spawn value; aarch64/riscv64
  paging tests assert the walk returns every drawn table exactly once, root
  last, leaves never; `FrameTableSource` tests pin reuse-after-free and
  double-free refusal; a `kthread` test pins the park-at-suspend; the
  spawn/wait/session QEMU verticals (all three ports) execute the real
  teardown at every child reap. Documented in
  `docs/src/architecture/memory.md`.
- **Follow-up (done):** the numeric free-memory-stability QEMU vertical
  (`tests/integration/memsoak_qemu_aarch64`, shared with I3 below). The
  `memsoak` fixture command app (`tests/integration/memsoak_program` —
  outside the userland discovery walk, so no production image ships it;
  composed/signed by the same xtask composer and planted only on that
  vertical's `FsDisk::MemsoakRootDisk` disk) runs from the scripted root
  shell and drives warmup + 32 measured spawn/wait cycles of `true.app`
  (the full reap-and-teardown path), asserting the final
  `KernelMemoryStats.free_bytes` — read through sysinfod's
  `KERNEL_MEMORY_STATS` query under its manifest's `CAP_SYSINFO_KERNEL` —
  equals the baseline **exactly** (the host-tested
  `rustos_test_memsoak::verdict`). On failure it prints `MEMSOAK FAIL
  baseline=… final=…` and parks forever, so the run times out fail-loud
  with the numbers in the transcript.

### I3. `top -d0` crashes the OS after ~1 h on a 180 MiB instance

**Status: done** (host reproduction green and every host-reachable layer
exonerated; the live/QEMU numeric re-test below is landed and green).

- **Defect:** a continuous-refresh `top -d0` session on a 180 MiB QEMU
  instance eventually crashed the OS; suspected out-of-memory.
- **Landed — the instrumented host reproduction.** Two standing regression
  soaks pin the cycle's memory behaviour, instrumented by the test-only
  opt-in counting allocator `kernel/core/src/test_alloc.rs` (per-measurement
  `LiveBytes` balances, immune to test parallelism):
  - `refresh_cycle_soak_retains_no_kernel_memory`
    (`kernel/core/src/syscalls.rs`) drives the exact per-refresh kernel
    sequence — timed `stream_read` whose bound elapses, `ipc_call` round
    trip against a live server thread, `sysinfo_introspect` process walk —
    for thousands of iterations and asserts zero retained bytes (±1 KiB
    sampling slack) and every ticket retired.
  - `refresh_shaped_workload_reaches_a_steady_mapped_extent`
    (`lib/rt/src/heap.rs`) replays the `top`/`sysinfod` allocation shape
    (doubling-realloc record vectors, paging scratch, small boxes, mixed
    frees) against the userland heap bookkeeping and asserts the mapped
    extent reaches a steady state and fully unmaps — a heap defect here
    would drain machine frames through `mem_map` from either process.
- **Findings:** the kernel side of the cycle retains nothing per iteration,
  and the userland heap is steady under the workload. The one growth the
  soak initially caught was its own fixture: the per-round-trip
  `CallPosted`/`CallReplied` audit records are `Level::Debug` **by design**
  and the production boot's `Info` filter drops them before any sink — only
  the test-global `Trace` filter plus a recording sink retained them. With
  the earlier suspects (`top`'s model, `CallEndpoint` bookkeeping, the
  write-through sinks) this exonerates every layer reachable from the host;
  the I2 teardown leak (now fixed) remains the prime explanation if the
  session had login/exit churn.
- **Landed — the live re-test** (`memsoak_qemu_aarch64`, shared with the
  I2 follow-up above): each measured cycle replays the `top -d0` refresh
  shape on the live production boot — a timed `stream_read` whose bound
  elapses (the tickless refresh park), a self-scoped process-list walk,
  and a `KERNEL_MEMORY_STATS` IPC round trip against the real `sysinfod`
  — on top of the spawn/reap, and the strict baseline/final `free_bytes`
  equality passes on QEMU. That reaches the layers the host soaks could
  not (arch port, UART TX path, timer/IRQ plumbing), so no live decay
  remains; a regression re-fails the vertical with the numbers in the
  transcript.

### I4. Pi 4B (8 GiB) reports 863 MiB — only the first `/memory` range is used

**Status: done** (verify the reported total on the 8 GiB board when
convenient), with one staged follow-up below.

- **Defect:** on an 8 GiB Raspberry Pi 4B the reported total memory was
  863 MiB: the aarch64 boot path read only `Fdt::first_memory_region()`, so
  just the first `/memory` `reg` window (below the BCM2711 MMIO hole)
  reached the frame allocator.
- **Landed behaviour:** `lib/fdt` exposes `Fdt::each_memory_region` (every
  `reg` pair of every top-level `/memory` node, honouring the root
  `#address-cells`/`#size-cells`, fail-closed on truncated pairs);
  `first_memory_region` is derived from it. The aarch64 boot collects all
  non-zero windows, clips them out of Device-typed gigapages
  (`mem_map::clip_windows_to_normal_ram` — the identity map types memory at
  1 GiB granularity and Device wins for a shared gigapage, so RAM sharing
  the UART/GIC/PCIe gigapage is dropped fail-closed), widens the RAM
  gigapage mask and live identity map per window, and
  `mem_map::build_memory_map` lays out N windows (kernel reserve + guard
  arena in the kernel's window, sized from *total* RAM; every other window
  wholly usable). Host tests cover the Pi 4-shaped multi-window/clip cases
  in `lib/fdt` and `mem_map`; `docs/src/platform/aarch64.md` describes the
  behaviour. On the 8 GiB board the reported total should now be ≈7 GiB.
- **Follow-up (staged):** reclaim the RAM clipped out of the Device-typed
  gigapage (~1 GiB on a Pi 4: `0xC000_0000..0xFC00_0000`) by typing the
  identity map at 2 MiB granularity inside a mixed gigapage; and adopt the
  all-ranges iterator in the riscv64 boot map builder
  (`riscv64::boot::build_boot_memory_map` still reads the first window —
  QEMU `virt` declares one, so nothing is currently lost there).

## 1. Everything is a bundle — including single-binary utilities

RustOS does **not** organise programs the Unix way: there is no `/usr/bin/<app>`
flat binary directory and no `man`-page directory in a separate tree
(`AGENTS.md` §16.1 forbids the legacy top-level names). Every program the user
can run — from a large graphical application down to a one-file utility like
`ps`, `top`, or `cat` — is an **application bundle**, a `<Name>.app` directory
whose fixed layout §16.5 defines.

A small single-binary utility is a perfectly good bundle: it has an `AppInfo`
manifest and a `Run` binary and little else. Keeping such tools as single
binaries inside a bundle is deliberate and is preserved — the bundle is the
*organisational* unit, not a demand that every tool grow extra machinery. The
same bundle shape scales up: a larger app adds `Code/`, `Libraries/`,
`Resources/`, and the internationalised `Help/` tree described below.

This applies to every present and future command-line program. A new CLI tool
is added as its own `<Name>.app` bundle (§16.5), never as a loose binary in a
shared directory.

### 1.1 Command surface follows GNU coreutils (`AGENTS.md` §16.7)

The OS-provided command apps (`ls`, `cat`, `cp`, `mv`, `rm`, `ps`, `top`, …)
MUST match **GNU coreutils** option names, argument grammar, and default
output as closely as possible (`AGENTS.md` §16.7): a user or script that knows
the GNU tool finds ours familiar, and any deviation carries the burden of
proof. RustOS-native concepts diverge deliberately and only where they
genuinely differ — capabilities (§5.2) instead of `setuid`, the storage forest
(§16.1) instead of Unix single-root paths, `Time64` (§21) timestamps, and the
System Information API (§16.6) instead of a fabricated `/proc`. The `stdinfo`
stream (§12, `AGENTS.md` §20) is **additive** on fd 3: a tool emits its
structured advisory records there in addition to its coreutils-compatible
stdout/stderr, never by reshaping stdout. Security and correctness (§5.4, §4)
win over bug-for-bug fidelity. The per-command option/output specifications
this document adds MUST honour §16.7.

## 2. Bundle layout (per §16.5)

The fixed top-level layout of `AGENTS.md` §16.5 carries one documentation
entry, `Help/` (the former `Documentation/`, merged into it):

```
/System/Apps/top.app/            # (or /Apps/Example.app for user apps)
├── AppInfo            # Signed manifest. Required.
├── Run                # Entry-point rxe binary. Required.
├── Code/              # Additional rxe binaries / plugins.
├── Libraries/         # Private shared libraries used only by this app.
├── Resources/         # Images, locales, UI definitions, etc.
├── DefaultSettings/   # Read-only defaults copied to the user on first launch.
└── Help/              # Internationalised Markdown help (this doc).
```

`Help/` is the bundle's **only** documentation mechanism — one
internationalised, structured-Markdown tree, so there is no second,
overlapping documentation entry to double-maintain (§2.2, §2.3). It is the
modern replacement for Unix man pages and the single source the CLI `man`
command (§7), each command's short `-h`/`-?` help (§4), and any graphical
help viewer read from. A bundle that ships longer-form material (a guide, a
tutorial) ships it as additional named *topics* in the same tree (§2.1),
rendered by the same engine.

Because `abi-v1` is not frozen (§9), the merge was a straightforward in-place
evolution (§2.13), and it has landed: `BundleEntry::Help` replaced
`Documentation` (`lib/abi/src/appinfo.rs`), `validate_bundle_layout` accepts
exactly the new set, every caller and fixture was updated in the same change,
and the generated C header carries `ROS_BUNDLE_ENTRY_HELP`.

The permitted top-level entry names remain a closed, case-sensitive set
validated by `validate_bundle_layout`: any entry outside the set still fails
the whole bundle closed (§5.4). `Help/` is a directory and is **optional** — a
bundle with no help still loads — but every OS-provided command app MUST ship
a `Help/` tree (§8 content policy).

### 2.1 The `Help/` locale tree

`Help/` contains one subdirectory per locale, of which the canonical
`en-US/` is mandatory:

```
top.app/Help/
├── en-US/             # The canonical source; MUST exist.
│   ├── top.md         # one Help document per command/topic
│   └── ...
├── fr-FR/
├── de-DE/
├── es-ES/
├── uk-UA/
├── it-IT/
├── pt-PT/
├── cy-GB/
├── zh-CN/
├── ja-JP/
├── ko-KR/
├── ar-SA/
└── he-IL/
```

- `en-US/` is the canonical help. It MUST exist for any bundle that ships
  `Help/`; a `Help/` tree without `en-US/` is a packaging defect and the
  loader/help engine fails closed (§5.4, §2.9).
- Each other directory is named by an exact BCP-47 tag and holds the same set
  of document file names as `en-US/`, translated.
- A locale directory MAY omit documents it has not translated yet; the help
  engine falls back per §5. It MUST NOT contain a document name absent from
  `en-US/` (there is nothing to fall back *from*, and it signals drift).
- One Help document describes one command or topic. A bundle whose `Run` (and
  `Code/`) expose several command names ships one document per command name,
  named `<command>.md`. The document for the bundle's primary command shares
  the command name (e.g. `top.md`).

## 3. Help document format

A Help document is a single UTF-8 Markdown file with a fixed, ordered set of
level-2 (`##`) sections. The section *keys* are language-neutral and fixed; the
prose under them is localised. This is what lets the help engine extract a
short synopsis for `-h` from the same file `man` renders in full, in any
language, without a per-language parser (§2.2).

Required and optional sections, in order:

| Section       | Required | Purpose                                            |
|---------------|----------|----------------------------------------------------|
| `NAME`        | yes      | Command name + one-line summary.                   |
| `SYNOPSIS`    | yes      | Usage line(s); option/argument grammar.            |
| `DESCRIPTION` | yes      | Full behaviour (the `man` body).                   |
| `OPTIONS`     | if any   | One entry per command-line switch (see below).     |
| `EXAMPLES`    | no       | Worked examples.                                   |
| `EXIT STATUS` | no       | Meaning of exit codes.                             |
| `ENVIRONMENT` | no       | Environment variables consulted.                   |
| `SEE ALSO`    | no       | Related commands (by command name).                |

Section keys are written in the document verbatim (`## NAME`), never
translated, so the engine locates sections structurally. Only the content is
localised.

### 3.1 Command switches are language-neutral

A command's **switches never change with the locale.** `top -d 0` is spelled
`top -d 0` in every language; `-d`, `-h`, `-?` are properties of the program's
argument parser, not of the help text. The `OPTIONS` section therefore records,
per switch, a language-neutral **key** (the literal flag, e.g. `-d`,
`--delay`) followed by localised description prose:

```markdown
## OPTIONS

- `-d, --delay <seconds>` — <localised description of the delay option>
- `-h, -?` — <localised description: show short help>
```

The flag tokens inside backticks are the single source of truth for the
switch spelling and MUST match the app's argument parser exactly. A CI check
(§8) verifies that every switch the program accepts appears in `en-US/`'s
`OPTIONS`, and vice-versa, so help and code cannot drift (§2.14, §2.18).

## 4. Two help surfaces: short (`-h`/`-?`) and full (`man`)

There are two ways to read a command app's help, both served from the one
`Help/` tree by the one help engine (§6):

- **Short help — `<cmd> -h` or `<cmd> -?`.** The program prints a concise,
  localised usage summary to `stdout`: the `NAME` and `SYNOPSIS`, plus the
  `OPTIONS` list rendered compactly. It fits a screen and is meant for "what
  are the flags again?". It exits `0`. `-h`/`-?` are reserved command switches
  every command app SHOULD accept; a program that defines no other meaning for
  them MUST treat them as short-help.
- **Full help — `man <cmd>`.** The `man` command app (§7) renders the whole
  Help document — every section — to the terminal with Markdown richness
  (headings, emphasis, lists, tables, code blocks), paged like the historical
  `man`, but from Markdown, in the user's locale.

Both surfaces select the same document for the same command; they differ only
in how much of it they render. Neither invents help text: if a section is
absent, it is simply not shown (§2.9, no fabrication).

## 5. Locale selection and fallback

The active locale is resolved once, by the session/shell, from the user's
language preference (a per-user setting under `/Users/<u>/Settings/`, surfaced
to programs as the **`LANG` environment variable**, a BCP-47 tag such as
`fr-FR` — the shell's existing `export` mechanism, `plans/SHELL.md`).
Programs and the help engine MUST NOT invent a second locale source. A
missing or malformed `LANG` selects the canonical `en-US/` documents: a
bad preference degrades to English, it never makes help unreadable.

Given a requested locale `ll-CC`, the help engine selects a document by the
first hit in this fixed, fail-safe chain:

1. `Help/ll-CC/<cmd>.md` — exact locale.
2. `Help/ll/<any-CC>/<cmd>.md` — same language, any region (deterministic:
   the lexicographically first matching directory, so the choice is stable).
3. `Help/en-US/<cmd>.md` — the canonical document.

If even `en-US/<cmd>.md` is absent, the engine reports "no help for `<cmd>`"
as an ordinary, non-fatal result (a clean message + non-zero status), never a
crash (§2.9). Falling back never mixes languages *within* a document: a
document is rendered whole from a single file.

## 6. The help engine — `lib/help`

There is exactly one help engine, the shared crate `lib/help` (`rustos-help`),
so `man`, every command app's `-h`, and any graphical help viewer share one
implementation (§2.2). Adding it updates `AGENTS.md` §3 and this plan (§6, §16.4
list) per the `lib/*` rules (§6).

`lib/help` is `no_std` + `alloc`, `#![forbid(unsafe_code)]`, and contains no
`unwrap`/`expect`/`panic!` on any path (§2.9). It:

- Locates a bundle's `Help/` tree and applies the §5 selection chain over an
  injected read-only file seam (it performs no ambient I/O; the caller supplies
  the capability-scoped reader, mirroring `appmgr`'s `BundleStore`).
- Parses the structured Markdown into the fixed §3 section model with **hard,
  fixed security bounds** (maximum document size, section count, nesting depth,
  line length, table size) that fail closed on violation (§24.4, §19.5). These
  are validation *bounds*, not growable capacities (§24.4).
- Extracts the short-help view (`NAME` + `SYNOPSIS` + compact `OPTIONS`) and
  renders the full view to the terminal through the `plans/CURSES.md` stack
  (`lib/vt`/`lib/curses`) — never a second escape-sequence vocabulary (§2.2).
- Treats help content as untrusted enough to be bounded and total even though
  it is signed (a malformed or hostile document degrades to a clean error, it
  never escapes its bounds), and ships a fuzz harness for the Markdown parser
  (§19.6).

`lib/help` is an internal building block, so it is linked **statically** by its
consumers (§16.4) — it is not one of the curated `/System/Libraries/` classes.

### 6.1 Help is authored once in the bundle — never embedded, never hand-listed

Help documents are **data on the volume**, not constants in a program. A
command's help lives in exactly one place — the bundle's own on-disk
`Help/<locale>/<doc>.md` files — and is read at runtime through the injected
`lib/help` `HelpSource` seam, from the running bundle's own `Help/` tree only.
This is binding under `AGENTS.md` §16.5:

- **No program embeds its own help.** A command app MUST NOT `include_str!` /
  `include_bytes!` its `Help/` tree into the `Run`/`Code/` binary, bake help
  strings into the program, or keep any second copy of a document outside the
  bundle. Short `-h`/`-?` help (§4) and `man` (§7) both read the same on-disk
  tree through the seam; the `Run` binary carries no help bytes of its own. A
  hand-written `help.rs` that embeds the documents is the defect this forbids.
- **The image builder discovers help, it does not list it.** The `Help/` trees
  are planted onto `/System/Apps/<name>.app/Help/` by `tools/mkimage` (and the
  QEMU image fixtures) from data discovered at build time by `tools/syshelp`,
  which scans the command-app bundles' own on-disk `Help/` sources. Adding a
  command app's help is dropping its `Help/` files under
  `userland/apps/<name>/Help/<locale>/`; the next build rediscovers them. No
  per-bundle list exists in the image builder, a fixture, or the kernel that a
  new bundle would force an edit to — that list would be the duplication §2.2
  forbids. `tools/syshelp` also fails closed on a document that does not parse
  under `lib/help`'s bounds or a bundle missing a required locale, so a
  malformed or partially-translated tree never reaches an image.
- **Internationalisation is the shared engine's job.** Locale fallback (§5) is
  the one `lib/help` chain (exact tag → same language any region → the
  canonical `en-US/`); a missing translation degrades to `en-US/`, never to
  fabricated or hardcoded text (§2.9).

## 7. The `man` command

`man` still exists, but it is RustOS'ised: it does **not** read the historical
troff/roff man format (RustOS ships none), it renders the `Help/` Markdown.

- `man` is itself a command app, `man.app`, in the system app store (§8) — it
  is not a shell builtin (it needs no shell-process state, `plans/SHELL.md`).
- `man <cmd>` resolves `<cmd>` through the **same** command-resolution path the
  shell uses (§9) to find the owning bundle, then renders that bundle's Help
  document for `<cmd>` in the active locale (§5) through `lib/help`.
- When the ordered store-then-`PATH` candidates find nothing for a bare word,
  `man` falls back to a **recursive bundle search** of the app stores: the
  machine-wide `/Apps`, then the user's own `<HOME>/Apps`
  (`rustos_cmdres::search_roots` — the roots are spelled once, over
  `rustos_abi::USER_APP_STORE`). The walk is breadth-first over sorted
  listings, so the shallowest match wins deterministically; it never
  descends into another bundle's `.app` directory (a bundle is a sealed
  unit); a missing root or directory lists nothing while any other refusal
  is final; and it is bounded (depth and a whole-invocation directory
  budget) — an exhausted budget is reported as a truncated search, never
  silently as "command not found". `man moose` therefore finds
  `/Apps/somefolder/anotherfolder/moose.app` or
  `/Users/<u>/Apps/somefolder/moose.app`. This search is `man`'s only:
  the shell's *launch* resolution (§8) is unchanged.
- `man <cmd> <topic>` selects `Help/<locale>/<topic>.md` within `<cmd>`'s
  bundle, for bundles that ship more than one topic.
- `man` emits a `stdinfo` `omission`/`context` record (fd 3, §20) when it falls
  back to a non-requested locale or to the canonical `en-US`, so a tool or
  user knows the page was not shown in the requested language. This never
  affects `man`'s exit status or output correctness (§20).

## 8. Command resolution — system app store then user `PATH`

Core/system command apps (`top`, `ps`, `ls`, `elsh`, `man`, …) MUST be
reachable simply by typing their command name. The shell resolves a bare
command word (after builtins, functions, and command aliases, per
`plans/SHELL.md`) in this fixed order:

1. **The system app store first.** The OS-provided command apps. Their store
   is a dedicated, read-only, system-signed location, `/System/Apps/` (an
   `AGENTS.md` §16.2 subdirectory — amendment applied, rationale in
   `PLAN.md` "Charter Amendments"), addressed by the `System:` path alias
   (`plans/DRIVES.md`). Its path and the bundle suffix are defined **once**
   in `lib/abi` (`SYSTEM_APP_STORE`, `BUNDLE_SUFFIX`), shared by the kernel's
   program registry (`kernel/rustos-kernel/src/spawn_paths.rs`, drift-tested)
   and the shell. The shell looks for `<word>.app` there and, if the manifest
   permits execution (§9.1), runs its `Run` binary through `appmgr`
   (signature + capability + interface-hash checks, §16.5).
2. **User `PATH` next.** The colon-separated directories in the shell's `PATH`
   environment variable (set by `export PATH=…` or a `.profile` in the user's
   home root), searched left to right. Because an alias path itself contains
   a `:` (`Home:/tools`), the split is structural and deterministic: a `:`
   immediately followed by `/` whose preceding text (since the previous
   separator) is a non-empty name containing no `/` is that entry's alias
   delimiter, not a separator — so an alias root entry is written `Home:/`,
   never a bare `Home:`. An empty entry is skipped (never a silent
   current-directory search). Each entry is resolved through the single
   shared path parser (`plans/DRIVES.md`), and each candidate is likewise a
   `<word>.app` bundle launched through `appmgr` — never a raw loose binary
   (§1).

The candidate *policy* is one pure, exhaustively-tested function,
`rustos_cmdres::resolution_candidates` (`lib/cmdres`, the shared crate whose
`bundle_candidates` view the `man` command's bundle lookup imports — one
policy, two views, §2.2/§17.4):
it computes only the ordered spelling list and grants nothing. The shell's
`Run` host attempts the candidates in order — the kernel's byte-exact
`spawn` lookup answering `NotFound` moves to the next candidate (a
deterministic first-match search, nothing ran), any other refusal is final
— and the kernel authorises every launch.

Searching the system store **before** `PATH` is a security property, not just
convenience: a user's `PATH` can never shadow a system command with an
attacker-supplied bundle of the same name. User-installed GUI/desktop apps live
in `/Apps` (§16.3) and are launched by the desktop/`appmgr`; they appear on the
shell command path only if the user explicitly adds `/Apps` (or a bundle path)
to `PATH`.

Resolution is deterministic and fails closed: an unresolved name is
`command not found` (`127`), and a resolved-but-non-executable bundle is
`command not executable` (`126`), matching `plans/SHELL.md`'s failure model
(implemented: the interpreter maps a launch `NotFound` onto `127` and every
other refusal onto `126`). No "try everything until one runs" behaviour
(§2.1).

### 8.1 Content and translation policy for OS help

- **Every OS-provided command app MUST ship a complete `Help/` tree**: an
  `en-US/` canonical document for every command it exposes, plus translations
  for the standing required locale set: `fr-FR`, `de-DE`, `es-ES`, `uk-UA`,
  `it-IT`, `pt-PT`, `cy-GB`, `zh-CN`, `ja-JP`, `ko-KR`, `ar-SA`, `he-IL`
  (the one definition is `rustos_help::REQUIRED_LOCALES`; every consumer —
  the lint and each app's own switch pins — imports it, never a private
  copy). The set gates *authoring* completeness only: runtime selection
  scans the bundle's `Help/` tree for whatever locales it actually ships
  (§5), so a third-party bundle with only `en-US/` still serves help.
  These documents MUST be generated and kept current; when an AI or a
  contributor changes a command's behaviour or switches, it updates the
  `en-US/` document and the translations in the same change (§2.8, §2.14,
  §2.18). Adding a language to the required set is data (a new locale
  directory), not new code.
- **No foul or derogatory content.** Help documents (all locales) MUST NOT
  contain profane, obscene, harassing, discriminatory, or otherwise derogatory
  language. This is a hard rule for generated and human-authored content alike.
- **Enforced in CI.** A `cargo xtask help-lint` check (run within
  `cargo xtask ci`, §7) fails closed when, for any OS command app: `en-US/`
  is missing or incomplete; a required-locale document is missing; the
  `OPTIONS` switch keys do not match the program's actual argument parser
  (§3.1); a document violates the `lib/help` structural bounds (§6); or a
  content-policy word-list/heuristic flags disallowed language. A lint failure
  is a defect fixed in the same change (§2.18), never waved through.

## 9. Invocation: `top` and `top.app`, and executability

A command app is runnable **both** by its bare command name and by its bundle
name:

- `top` — the command name; resolved per §8.
- `top.app` — the bundle name; the shell recognises a trailing `.app` on a
  command word, resolves the bundle directly, and runs it identically.

Both forms run the same `Run` binary through `appmgr` and are subject to the
identical signature and capability checks (§16.5); the `.app` spelling is a
convenience, never a privileged bypass (§5.4).

### 9.1 The manifest gates executability

Whether a bundle is a command app at all is decided by its **signed
`AppInfo` manifest**, not by its file name. A bundle is executable as a command
only if its manifest declares a runnable entry point and the launching user's
grants intersect the manifest's requested capabilities to a non-empty, valid
set (§16.5, §5.2). A bundle that declares itself non-executable (a
resource-only bundle, §10) is refused as a command (`126`) even if a user types
its name or `<name>.app`.

## 10. Resource-only ("shared-resources") bundles

A bundle MAY declare in its manifest that it is **resource-only**: it has no
runnable command entry point and exists to hold shared *data* (fonts, icons,
locale packs, templates, help topics) for a family of apps from one publisher.
Such a bundle still carries and enforces every §16.5 security guarantee: it is
signed, its layout is validated, and access to its contents is
capability-gated (§5.4). Attempting to *execute* it fails closed (§9.1).

**What a resource-only bundle MUST NOT do: provide shared dynamically-linked
libraries to *other* bundles.** `AGENTS.md` §16.4 is explicit and binding: the
dynamic loader refuses any shared-library reference outside the requesting
app's own `Libraries/` or the curated `/System/Libraries/`. A
"shared-resources.app" that other apps dynamically link against for **code** is
therefore not permitted, and the loader fails such a reference closed — this
spec does not carve an exception, because §16.4 wins (§2.13 forbids adding a
compatibility seam around it).

The compliant ways a single publisher shares code across their own apps are:

1. **Vendor the library into each app's own `Libraries/`** (statically, or as a
   bundle-private dynamic library the loader already permits, §16.4). One
   security update per app; the publisher rebuilds and re-signs.
2. **Promote the code to a curated `/System/Libraries/` class**, if and only if
   it genuinely belongs to one of that closed set (§16.4) — which requires an
   `AGENTS.md` §16.4 amendment and is an OS decision, not a third-party one.

Shared **data** (not code) is what a resource-only bundle legitimately
provides, reached through capability-gated file access (a manifest-declared or
user-mediated file capability, §16.5), never through the dynamic loader.

## 11. Security summary

Every mechanism here obeys the charter's fail-closed, least-authority model:

- **Signed, verified, capability-gated launch.** Resolving and running a
  command app — from the store or `PATH`, as `top` or `top.app` — always goes
  through `appmgr`'s signature, content-hash, interface-hash, and capability
  intersection checks (§16.5, §5.2). No path bypasses them.
- **System store precedes `PATH`.** A user cannot shadow a system command
  (§8).
- **Help content is bounded and total.** `lib/help` parses Markdown under fixed
  security bounds and never crashes on malformed input, with a fuzz harness
  (§6, §19.5, §19.6).
- **No ambient authority.** `lib/help` and `man` perform I/O only through
  injected, capability-scoped seams; help never reads outside the target
  bundle's `Help/` tree (§4, §5.4).
- **No fabricated content.** Missing sections/documents/locales degrade to
  clean messages, never invented text (§2.9).

## 12. Structured advisory output (`stdinfo`, fd 3)

Command apps SHOULD support the standard information stream (`stdinfo`, fd 3,
`AGENTS.md` §20 / §20.1) **wherever it is meaningful**. Whenever a command
hides, filters, truncates, or summarises its primary `stdout`, or when concise
non-obvious context would help a human or an AI/tool interpret the output, it
SHOULD emit the appropriate framed `StdInfoRecord` (`lib/abi/src/stdinfo.rs`,
via the `lib/rt` `stdinfo` wrapper — never a device syscall, §20) using one of
the closed canonical `kind` values (`omission`, `summary`, `schema`,
`suggestion`, `context`). It is optional and ignorable by construction:

- `stdinfo` is **advisory only** and MUST NOT affect correctness, exit status,
  scripting semantics, or pipeline behaviour (§20.1). A `stdinfo` write
  failure never changes `$?` (`plans/SHELL.md`).
- It is emitted best-effort and non-blocking when no consumer is attached
  (§20.1); a program with no fd 3 attached simply proceeds.
- The help surfaces here already use it: the `man` command emits an
  `omission`/`context` record on a locale fallback (§7), and a command's
  short help (§4) MAY note omitted detail the same way.
- Records MUST stay terse, actionable, and free of the content §20.1 forbids
  (progress spam, secrets, capability tokens, security/audit events — those go
  to `lib/log`, §19.4 — or instructions to AI agents). Consumers treat
  `stdinfo` as untrusted data about the command, never as authority (§20.1).

Where a command has nothing non-obvious to add, it emits nothing: `stdinfo` is
a channel for *useful* advisory metadata, not a requirement to speak on every
invocation.

## 12.1 GNU coreutils parity — staged plan (§1.1 / `AGENTS.md` §16.7)

The maintainer-approved staging that brings every `/System/Apps` command to
the GNU coreutils surface and fills in the missing commands. Each stage is
one properly-gated change; a stage's switch set is bounded by what the
platform floor can honestly implement (userland `FileStat` carries
kind/size/mode/uid/gid only — no timestamps, and the VFS has no symbolic or
hard links — so time- and link-dependent behaviour is deliberately staged
behind the floor work below).

- **Stage A — GNU switches for the existing tools (done).** On the current
  floor: `cat` `-A -b -e -E -n -s -t -T -u -v` (bundled short flags, GNU
  `^`/`M-` notation); `ls` `-a -A -d -F -g -h -l -m -n -o -p -Q -r -R -S -1`
  (`-h` takes the GNU human-readable meaning — short help is `-?`/`--help` —
  and the invented `--long` synonym is retired; long format shows numeric
  owner/group, the GNU numeric fallback, with no link-count/timestamp
  columns until the floor carries them); `rm` `-d -f -i -I -r -v
  --preserve-root`/`--no-preserve-root` (prompt seam, GNU `removed …`
  wording); `cp` `-f -i -n -r -t -T -v`; `mv` `-f -i -n -t -T -v`
  (`-f`/`-i`/`-n` last-wins, `renamed 'a' -> 'b'` wording); `chmod` and
  `chown` `-c -f -v` (GNU changed/retained wording; `-f` suppresses
  per-operand diagnostics, continues, and still fails the run via the
  message-less `Silenced` error). Each tool's `Prompt`/`Output` seams stay
  injected and fail closed; registered tools' thirteen-locale `Help/` trees and
  the switch-drift pins are current.
- **Stage B — register the orphan utilities as store bundles (in
  progress).** Each orphan gains `AppInfo.toml`/`Run`/`Help/` (the §8.1
  required locales) and store registration per §16.5/§6.1, wiring the `Prompt`
  seam to stderr+stdin (`y`/`Y` affirmative; end-of-input is a decline,
  never consent) in each `Run` host. **Done: `cp`, `mv`, `rm`** — each is
  a full self-contained store bundle (console pair + `CAP_FS_ACCESS`
  request, store-only: the §18.6 boot floor never grows, so the kernel
  inventory drift test pins their `AppInfo.toml` directly with no
  embedded registry row), with `-h`/`-?`/`--help` short help over the
  shared `own_short_help`/`BundleHelp` render and per-locale switch-drift
  pins. Landing `mv` added the missing `EXDEV` equivalent in place
  (`abi-v1` unfrozen): the dedicated `Errno::CrossVolume` /
  `VfsError::CrossVolume` a cross-mount `fs_rename` is refused with
  (regression-tested; C header regenerated), so `mv`'s copy-then-remove
  fallback triggers on exactly that condition and no other.
  **Done: `useradd`, `groupadd`** — store-only bundles over the existing
  `users_admin` syscall (`CAP_CONSOLE_WRITE` + `CAP_USER_ADMIN` +
  `CAP_FS_ACCESS`; no console-read — they never prompt), each with a
  host-tested production `users_admin` client behind injected
  channel/entropy seams. The shared account-authoring policy
  (`DEFAULT_SHELL`, `default_home`, `next_id` id auto-allocation) was
  hoisted into `lib/users` and the `users` session + `tools/mkimage`
  deduplicated onto it. `useradd` creates the account with an unusable
  random password record (the GNU `!`-field equivalent — a password is
  set afterwards via the `users` tool), the session-baseline ceiling,
  and the shared shell/home defaults.
  **Done: `chmod`** — registered in the change that landed its syscall:
  `fs_set_mode` (74, the `chmod(2)` shape; `CAP_FS_ACCESS`-gated, audited,
  mode word validated to `FS_MODE_MASK` at dispatch and again at the
  service seam, never masked) flows dispatcher → `MountedFilesystemService`
  → `Vfs::set_mode_via_secured` → the per-inode `DelegatedFs::set_mode`,
  where only the inode's **owner** may change its mode (no
  write-implies-chmod, no capability override, `required_cap` honoured,
  read-only mounts refused), rewriting only the driver security record's
  mode field. The bundle is store-only (console-write + `CAP_FS_ACCESS`),
  with the thirteen-locale `Help/` tree, `-h`/`-?` over `lib/help`, and
  the `ros_sys_fs_set_mode` stub + `ROS_FS_MODE_MASK` in the regenerated C
  header. `fstree`'s `a` mode editor is the second caller.
  **Remaining, blocked on kernel prerequisites:** `chown` needs the fs
  owner-set syscall, `getcap`/`setcap` need per-inode
  capability-requirement get/set syscalls, and `mount` needs the mount
  syscall — none exists yet, and a stubbed production seam is forbidden;
  each tool registers in the change that lands its syscall.
- **Stage C — missing coreutils commands (in progress; all wanted).**
  Every GNU coreutils command implementable on the current floor, in
  prioritised batches. **Done: `true`, `false`, `yes`, `basename`,
  `dirname`, `mkdir`, `rmdir`, `head`, `wc`, `tee`, `seq`, `whoami`,
  `du`, `df`, `printf`** — each a
  full self-contained store bundle (console-write +
  `CAP_FS_ACCESS` request — plus console-read for the stdin-reading
  `head`/`wc`/`tee` — store-only: the §18.6 boot floor never grows,
  so the kernel inventory drift test pins their `AppInfo.toml` directly)
  with the complete GNU surface, thirteen-locale `Help/` trees, and
  switch-drift pins. `true`/`false` parse infallibly (only a *first*
  argument of `-h`/`-?`/`--help` is honoured, the GNU position rule);
  the one documented divergence is that `false`'s served short help
  exits `0` per §4, where GNU `false --help` exits `1`. `basename` and
  `dirname` are purely lexical, with one shared RustOS extension: a
  `Name:/` alias root plays the role POSIX gives `/`, decided by the
  path grammar's own exported rule (`rustos_path::alias_root_len`, the
  §2.2 one-definition seam added for exactly these lexical tools) —
  never a second path parser. Landing `mkdir`/`rmdir` evolved `abi-v1`
  in place (unfrozen, the `mv`/`CrossVolume` precedent): the dedicated
  `Errno::NotADirectory`/`Errno::NotEmpty` codes (`VfsError` now maps
  `AlreadyExists`/`NotADirectory`/`NotEmpty` precisely instead of
  collapsing them onto `OutOfRange`; C header regenerated with the new
  errnos, `ROS_UNLINK_FLAG_DIRECTORY`, and the previously-unpublished
  `ROS_OPEN_FLAG_*` bits) and a validated `UnlinkFlags` word on
  `fs_unlink` whose `DIRECTORY` bit is the atomic
  `rmdir(2)`/`unlinkat(AT_REMOVEDIR)` posture, decided by the
  filesystem under its own lock — `rmdir` (and `rm`'s own directory
  removals, migrated to the flag) carries no stat/remove race, and
  `--ignore-fail-on-non-empty` tolerates exactly `NotEmpty`. Both
  tools' `-p` walks share the one ancestor-spelling rule
  (`rustos_path::Path::prefix`, the §2.2 seam added for exactly these
  walks); `mkdir`'s GNU `-m` remains staged — its kernel prerequisite
  (`fs_set_mode`, Stage B `chmod`) now exists, and the flag lands with
  its own tests in its own change, never stubbed. `head`
  implements the full GNU surface — `-n`/`-c` with the leading `-`
  elide form and the multiplier suffix alphabet, `-q`/`-v`/`-z`,
  bundles/permutation, and the obsolete first-argument
  `-COUNT[bkm][lqvz]` form (including GNU's quirk that a multiplier
  letter keeps scaling after a later `l`) — streaming in constant
  memory (a circular byte ring for `-c -N`; a last-N-lines queue whose
  unterminated final fragment counts as a line). `wc` implements
  `-c`/`-m`/`-l`/`-w`/`-L`, `--total` (GNU argmatch prefixes), and
  `--files0-from` with the exact GNU column-width rule (summed
  regular-file sizes via the three-way `SizeProbe` seam; 7-column
  minimum for non-regular inputs; unpadded single-input/single-count,
  files0, and total-only forms); `-m` decodes UTF-8 incrementally
  across chunks (an encoding-error byte is a byte, not a character)
  and `-L` measures columns through the one `rustos_vt::char_width`
  definition — never a second width table. `tee` implements
  `-a`/`--append`, `-p`, and `--output-error[=MODE]` (argmatch
  prefixes; the value only attached with `=`, a bare `--output-error`
  selecting `warn-nopipe`) with the GNU `tee.c` failure discipline (a
  failed output is diagnosed, dropped, and the run continues or stops
  per mode; reading stops when no output remains). Two documented
  divergences: RustOS has no `SIGPIPE`, so the modes' "pipe" class maps
  to the standard-output copy (the one output that can be a pipe; the
  default mode's stdout failure stops the run fail-loud), and
  `-i`/`--ignore-interrupts` is staged behind per-process
  signal-disposition kernel work (nothing exists to set today), never
  stubbed — the `mkdir -m` precedent. `seq` implements the full GNU
  surface — `-f`/`--format` (a validated one-directive printf float
  format rendered by a C-locale `%e`/`%f`/`%g`/`%a` engine),
  `-s`/`--separator`, `-w`/`--equal-width`, GNU operand scanning (no
  permutation, negative-number operands), the spelling-derived default
  precision/width, the exact decimal fast path (arbitrary-size integer
  runs, `inf` LAST), and the extra-number rounding rule — with one
  documented divergence: the float path computes in `f64`, not glibc's
  `long double` (visibly, `%a` prints the `double` spelling `0x1.8p+0`
  rather than `%La`'s `0xcp-3`). `printf` implements the full GNU
  surface — the escape set (`\NNN`/`\xHH`/`\uHHHH`/`\UHHHHHHHH`, `\c`
  ending all output), every conversion (`diouxX`, `eEfFgGaA`, `%c`,
  `%s`, `%b` with `\0NNN` octal, `%q` quotearg-style shell quoting,
  `%%`) with the C flags and `*`-settable width/precision, format
  reuse, base-0/char-constant argument reading, and the GNU
  diagnostics and exit model (conversion errors continue and exit `1`;
  an invalid conversion specification or malformed escape is fatal
  with prior output kept; the per-conversion flag-validity table is
  probe-pinned against GNU coreutils). Landing it hoisted the two
  C-locale engines `seq` already carried into `lib/util` for the
  second consumer: `rustos_util::cfloat` (the printf float renderer
  behind `seq -f` and `printf`'s float conversions) and
  `rustos_util::cnum` (the `strtod` scanner, now with longest-prefix
  `endptr` semantics — `seq` demands full consumption, `printf`
  diagnoses the remainder). Two documented divergences: the
  `f64`-not-`long double` computation (the `seq` precedent) and the
  RustOS first-argument `-h`/`-?`/`--help` short-help convention
  (`printf -- -h…` spells such a format). `whoami` prints the name paired with
  the caller's uid: the uid comes from the kernel-attested origin
  record (the ungated `self_origin` syscall) and the name from the
  ungated `USER_DIRECTORY` sysinfod query over the shared
  `rustos_procinfo` account-directory walk (the `top` USER-column
  helper, one definition); a uid with no directory entry is the GNU
  `cannot find name for user ID` diagnostic, and a failed walk is a
  service error, never misreported as a missing name. `du` walks its
  operands post-order over the `fs_*` seams with an explicit frame
  stack, measuring each node's `fs_stat` `allocated` bytes by default
  (`--apparent-size`/`-b` for lengths) and implementing `-a`/`-s`/`-c`/
  `-d`/`-S`/`-0` plus the GNU unit options; the `-B` grammar, ceiling
  block scaling, and `human_ceiling` renderings live once in
  `rustos_util::size` (a §2.2 `lib/util` promotion, shared with `df`).
  `df` renders the GNU table (auto-sized columns, `-a`/`-T`/`-t`/`-x`/
  `-i`/`-P`/`-l`/`--total`, operand→covering-mount by longest prefix)
  from the ungated `MOUNT_LIST` rows, which now carry each volume's
  `VolumeStats` — the support work: a new versioned `FilesystemStats`
  driver-ABI extension (`stats() -> VolumeStats`, rustfs reports its
  live accounting with the metadata reserve withheld from
  `avail_blocks` and the honest zero inode pair), `MountRecord` evolved
  in place to embed the usage block (invariant-checked on construct and
  decode; C header regenerated), and the kernel mount snapshot now
  reporting each backed mount's registration `source`/`fstype` names
  (`LateFilesystem::register` carries them) plus the driver's live
  stats — an unbacked mount truthfully reports empty names and the
  all-zero usage, which `df` hides by default and notes on fd 3
  (`fs.mounts_omitted`). Documented divergences: `du` has no link
  deduplication (no hard links yet, Stage E) and no `-x` (no device
  identity); `df` stages `--output`/`--sync`; neither reads the
  `*_BLOCK_SIZE` environment family. `echo` and `pwd`
  from the first batch
  stay `elsh` builtins for now (`pwd` needs the shell's cwd state, and
  the shell resolves builtins first, so a store bundle of either would
  be unreachable duplication); moving them out is decided when the
  builtin/bundle split is revisited. Remaining first batch:
  `tail`, `env`, `sleep`,
  `date`, `id`; then the text tools `sort`,
  `uniq`, `tr`, `cut`, `paste`, `comm`, `nl`, `tac`, `fold`, `expand`,
  `od`, `split`, `shuf`, `truncate`, `mktemp`, `realpath`, `chgrp`,
  `sha256sum`/`cksum`, `base64`). Each is a full self-contained
  bundle (§16.5) with tests and a thirteen-locale `Help/` tree; anything a
  shell builtin duplicates moves out of `elsh` where applicable.
- **Stage D — filesystem timestamps (planned).** Plumb the driver-level
  `NodeTimes` (four `Time64` stamps) through `fs_stat`/`FileStat` (an
  in-place `abi-v1` evolution), then `touch`, `ls -l` dates and `-t`/`-u`/
  `-c` sorts, `cp -p`/`-u`, `mv -u`, `date -r`.
- **Stage E — VFS links (planned; separate design).** Symbolic/hard-link
  support in the VFS and RustFS, then `ln`, `readlink`, `link`, `unlink`,
  `ls -L/-H`, `cp -l/-s/-d/-a`, `stat`'s link fields.

## 13. Deliverables and required `AGENTS.md` amendments

Staged work (dependencies: the bundle/`appmgr` stack and `plans/CURSES.md`,
both landed; `plans/SHELL.md` command execution):

1. **`lib/abi` — `BundleEntry::Help`** — **done.** The maintainer chose the
   merge, so `Documentation` was renamed to `Help` in place (§2.13):
   enum/`ALL`/`as_str`, rustdoc, the `appinfo` and `appmgr` fixtures,
   `docs/src/abi/appinfo.md`, and the regenerated C header
   (`ROS_BUNDLE_ENTRY_HELP`); the retired name now fails
   `validate_bundle_layout` closed.
2. **`lib/help` (`rustos-help`)** — **done.** The one help engine (§6):
   validated `Locale`/`DocumentName` spellings, the injected `HelpSource`
   read seam, the §5 fallback chain (served locale reported for `stdinfo`),
   the bounded structured-Markdown parser (fixed §3 section model, typed
   `HelpError`, fence-aware section walk), and `render_short`/`render_full`
   over `lib/vt` (widths from `lib/curses`). It also owns a command's
   **own** §4 short help in one place: the pure `own_short_help` render
   (LANG parse, load, short view, `lib/vt` bytes; `None` falls back to the
   caller's usage banner) and the `rt`-feature `BundleHelp` production
   source over the app's own store bundle — shared by every `Run` binary,
   never re-derived per tool. Unit tests, the `fuzz_help`
   harness registered in `cargo xtask fuzz` (§19.6), rustdoc,
   `lib/help/README.md`, `docs/src/lib/help.md`, and the §3 crate list are
   in place.
3. **`man.app`** — **done.** The RustOS `man` command app (§7):
   `userland/apps/man` resolves the word over `rustos_cmdres::
   bundle_candidates` (first existing bundle wins; `NotFound` moves on, any
   other refusal is final) and then, for a bare word no candidate matched,
   over the §7 bounded recursive search of `/Apps` and `<HOME>/Apps`
   (`rustos_cmdres::search_roots`), loads and renders through `lib/help`,
   reads `LANG`/`PATH`/`HOME` from the inherited environment, pages on a
   geometry-attested console (space/return/`q`, echo suppressed) and
   streams otherwise, and emits the §7 `stdinfo` `context` record
   (`help.locale_fallback`) on a locale fallback. Registered as
   `/System/Apps/man.app/Run` (manifest: console pair + `CAP_FS_ACCESS`);
   its own thirteen-locale `Help/` tree is authored on disk in the bundle and
   read at runtime through the `BundleStore` seam (no help embedded in the
   binary, §6.1) — the tree is discovered by `tools/syshelp` and planted on
   the read-only `/System` volume by `tools/mkimage` and the QEMU image
   fixture; the `session_ceiling` vertical types `man man` end to end.
4. **Shell command resolution** — **done**:
   system-app-store-then-`PATH` resolution (§8) and `.app`-suffix invocation
   (§9) are live. The store/bundle spellings live once in `lib/abi`
   (`SYSTEM_APP_STORE`/`BUNDLE_SUFFIX`); every OS command app is registered
   as `/System/Apps/{cat,clear,elsh,ls,man,ps,reset,sysinfo,top,users}.app/Run`
   (`spawn_paths.rs`, drift-tested); the pure candidate policy
   (`rustos_cmdres::resolution_candidates` in the shared `lib/cmdres`
   crate, alias-aware `PATH` split, plus the `bundle_candidates` view for
   `man`'s bundle lookup) is unit-tested; the interpreter maps launch
   failures onto `127`/`126`; the shell passes the typed words and
   exported environment to every launched program over the `spawn`
   startup-strings block (`plans/SPAWN.md` SP8 — the §5 locale variable
   and `man <cmd>`'s argument now reach a child); and the session-ceiling
   QEMU vertical proves the bare word `ps` **and** a delivered `ps
   --bogus` argument end to end.
5. **`cargo xtask help-lint`** — **done.** The §8.1 gate, wired into
   `cargo xtask ci` (and `ci-long`) among the cheap fail-fast checks. The
   judgement is one definition, `rustos_help::lint_help_trees` (`lib/help`'s
   host-only `lint` cargo feature; pure rows-in/violations-out, unit-tested
   per violation class), shared by the gate and the `tools/syshelp`
   aggregator tests (§2.2) so the two can never diverge. It checks, over the
   build-discovered `rustos_syshelp::HELP_FILES` rows (the same data the
   image planters plant): locale/document spellings and the `lib/help`
   structural bounds (§6), `en-US/` presence and completeness across the
   standing `rustos_help::REQUIRED_LOCALES` set, no translation-only
   documents (§2.1), per-item backticked switch keys and cross-locale
   `OPTIONS` key equality against `en-US/` (§3.1 — the per-app unit tests
   keep pinning `en-US/` to each parser, which only the app crate knows),
   and the closed content-policy screen (whole-word, case-insensitive, plus
   the CJK substring screen, over every locale). The gate additionally
   verifies coverage: every command
   app the `AppInfo.toml` discovery walk finds ships its
   `en-US/<command>.md` (never a per-bundle list). Any violation fails
   closed with a message naming the offending `bundle/locale/file`.
6. **`Help/` trees for the existing command apps** — **done for every
   store-registered command app**: `basename`, `cat`, `clear`, `cp`,
   `dirname`, `edit`, `false`, `groupadd`, `head`, `ls`, `mkdir`,
   `mv`, `ps`, `reset`, `rm`, `rmdir`, `seq`, `tee`, `top`, `true`, `sysinfo`, `useradd`,
   `users`, `wc`, `whoami`, `yes`, and `elsh` each author their thirteen-locale tree on disk in the bundle,
   discovered by `tools/syshelp` (roots `userland/apps` and
   `userland/shell`), planted at `/System/Apps/<cmd>.app/Help/`, and served
   at runtime through the `HelpSource` seam — never embedded in the binary
   (§6.1) — for each tool's §4 `-h`/`-?` short help (a per-locale
   switch-drift unit test pins each tree's `OPTIONS` to its parser, §3.1).
   The tools that gained filesystem reach for that read request
   `CAP_FS_ACCESS` in their manifests (the man/ls precedent — the secured
   VFS still authorises per-inode). The not-yet-registered utilities
   (`chmod`, `chown`, `mount`, `getcap`,
   `setcap`, `terminal`, …) gain their trees in the
   same change that registers each as a store bundle (§12.1 Stage B). Each new tree ships
   by dropping its `Help/` files under the bundle — `tools/syshelp`
   rediscovers them, and no image-builder list is edited (§6.1).

7. **`stdinfo` adoption in command apps (§12)** — emit the appropriate
   `StdInfoRecord` (via the `lib/rt` wrapper) wherever a command omits,
   summarises, or adds non-obvious context to `stdout`. Live: `man`'s
   locale-fallback record (§7), `ls`'s `fs.hidden_entries_omitted`
   omission record (the `AGENTS.md` §20.1 canonical example), and the
   `proc.self_scope_only` omission record both `ps` and
   `sysinfo processes` emit on their default self-scoped listing (one
   shared definition, `rustos_procinfo::emit_self_scope_omission`,
   parametrised only by each tool's widening spelling — `ps -e` /
   `sysinfo processes --all` — over the shared
   `rustos_procinfo::Output::info` fd 3 seam, fail-closed on an argv
   token that would break the record's JSON); advisory-only, never
   changing exit status. The remaining registered commands were surveyed
   and have nothing non-obvious to add today: `cat` and `users` omit
   nothing from stdout, and a per-refresh record from full-screen `top`
   would be the progress spam §20.1 forbids. A future command adds its
   record in the change that creates the omission/summary, through a
   shared definition when two tools share the behaviour.

8. **Self-contained on-disk bundles — retire the kernel-baked spawn
   registry (§16.5 self-containment and §16.2 services-are-apps
   amended).** Every discovered bundle — the command apps *and* the
   `login`/`devmgr`/`sysinfod` services (a service is an app, §16.2) —
   ships complete on the read-only `/System` volume: its signed `AppInfo`
   + `Run` rxe beside its `Help/` tree, at the bundle-form paths
   (`/System/Apps/<cmd>.app/`, `/System/Services/<name>.app/`) PID 1
   `init` and the shell name. The binding increment list, with
   per-increment status and the load-bearing invariants, lives in
   `PLAN.md` ("Self-contained bundles"). Increments 1–4 are **done**: the
   canonical bundle content-hash framing (`lib/abi`), the per-crate
   `AppInfo.toml` discovery walk + composer signing under the dedicated
   `SYSTEM_APP_SIGNING_SEED`, the image/fixture planting of every signed
   bundle, the shared `lib/appload` verification engine (`appmgr`
   re-exports it), and the `spawn` syscall loading + verifying store
   bundles from the mounted volume through `rustos_appload` — deriving the
   child's capability request from the on-disk manifest, parking a
   boot-race spawn on the `AppStore` readiness latch, and dropping every
   embedded rxe row from the aarch64 production boot (the system
   principal resolves via the bootstrap identity before the root unlock,
   so PID 1 can load the service bundles pre-passphrase). Verification
   runs **once per boot** per read-only store bundle: the accepted
   result is cached in the `AppStore` (LRU under a
   discovered-RAM-fraction byte budget) and a later launch serves the
   cached image after re-authorising the caller's read of `Run` through
   the secured VFS, so command-launch latency stays off the whole-bundle
   hash/signature path; writable-volume bundles are never cached.
   **Remaining:**
   increment 5 — the x86_64/riscv64 storage floor, then deletion of
   `SPAWN_PROGRAMS`, the `*_rxe.rs` `include!`s (all but PID 1 `init`),
   `spawn_paths.rs`, and `program_manifests.rs` (§2.14); until then those
   two ports carry the embedded registry as their explicitly-justified
   §18.6 boot floor. All prior deliverables' references to
   `/System/Apps/<cmd>.app/Run` being served from `spawn_paths.rs` are
   superseded by this on-disk-bundle model.

Required `AGENTS.md` amendments (each with a one-line rationale in PLAN.md's
"Charter Amendments" section, §13):

- **§16.5** — **done**: `Documentation/` replaced by `Help/` in the bundle
  layout (the merge), with the locale-tree role documented; rationale logged
  in PLAN.md "Charter Amendments".
- **§16.2** — **done**: `Apps/` added under `/System` as the read-only,
  system-signed system app store (§8), in the §16.2 authoritative
  subdirectory list; rationale logged in `PLAN.md` "Charter Amendments".
- **§16.6/§5.2** — **done**: no new capability is introduced for help or
  command resolution (existing file-access and driver/app-load gates
  suffice, §5.2 minimalism); stated explicitly in the §16.2 `Apps/` entry so
  none is added speculatively.
- **§16.5 (help authoring)** — **done**: added the binding rule that command
  help is authored once in the bundle's on-disk `Help/` tree and read at
  runtime through the `lib/help` seam — never embedded/compiled into a
  program (`include_str!`/`include_bytes!`/baked strings) and never planted
  from a hand-maintained per-bundle list in the image builder. The build
  discovers the trees (`tools/syshelp`, added to §3); the per-app embedded
  `help.rs` copies and the mkimage/fixture lists were deleted (§2.2, §2.14).
  Rationale logged in `PLAN.md` "Charter Amendments".
- **§16.2 (services are apps)** — **done**: a `/System/Services/` service
  ships as the same self-contained, signed `<name>.app` bundle as any app,
  discovered from disk and loaded through the identical verification gate;
  only PID 1 `init` is the compiled-in boot floor. Rationale logged in
  `PLAN.md` "Charter Amendments".
- **§16.5 (self-containment) / §16.2 (`/System/Apps`)** — **done** (charter);
  **code migration open (deliverable 8).** Added the binding rule that an
  app *is* its `<Name>.app/` bundle directory: `Run`, `Code/`, `AppInfo`,
  `Resources/`, `DefaultSettings/`, `Help/`, and any app-private static or
  shared library are all real files inside the folder, discovered from disk;
  app code is never compiled into or served from the kernel/image builder,
  and the store is never a compiled-in registry. The only outside reach is
  the curated `/System/Libraries/` set and the syscall ABI. Rationale logged
  in `PLAN.md` "Charter Amendments"; the code migration off the kernel-baked
  spawn registry is deliverable 8 above.
