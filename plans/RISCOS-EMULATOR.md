# RISCOS-EMULATOR — A RISC OS !Application emulator, delivered as an `.app`

Status: **planned** (design fixed below; no code landed).

This document is the standalone build plan for running **RISC OS
!Applications** on TAIRiX inside a self-contained emulator that ships as an
ordinary TAIRiX application bundle. It is **binding under `AGENTS.md`** the
moment implementation starts. Until then it is deliberately *unlinked*: no
`AGENTS.md`, `PLAN.md`, `plans/` jump-sheet, or other document references it.
The first implementation change wires it in (`AGENTS.md` §15.18 table row,
`PLAN.md` stage, and any §3 / §16.4 layout updates a new crate forces).

`AGENTS.md` is binding and wins over this document wherever they disagree.

## 0. What is being built, and what is emphatically *not*

- **A RISC OS emulator, delivered as a TAIRiX `.app` (§16.5).** The emulator
  is a normal signed bundle under `/Apps` (e.g. `/Apps/RISCOSEmulator.app`).
  It is **not** wired into the kernel, a driver, a system service, or the
  desktop session. Nothing in `kernel/*`, `drivers/*`, or `userland/system/*`
  gains an edge to it. It runs as an unprivileged user process holding only
  the capabilities its manifest requests ∩ the launching user's grants (§4,
  §5.2).
- **RISC OS is a *guest*, not TAIRiX.** Everything RISC OS — its SWIs, its
  memory model, its 26/32-bit ARM state, its WIMP, its filing system view —
  is emulated *inside the emulator process*. None of it is a TAIRiX kernel
  facility. Do not confuse a RISC OS concept with a TAIRiX one: a RISC OS
  "module", "SWI", "Wimp task", or "filetype" is guest state the emulator
  owns, never a TAIRiX syscall, capability, or driver. The one genuine
  crossing point is the OS-level *file-run delegation* prerequisite (§2), a
  general TAIRiX facility the emulator merely happens to be the first
  consumer of.
- **The insecure RISC OS SWIs stay inside the emulator (§5, §19).** Almost
  every core RISC OS SWI (raw memory pokes, `OS_CLI` shelling out, unchecked
  filing-system access, direct hardware/vector claims) would be a gaping hole
  in a modern OS. That is *exactly why* they are implemented by the emulator
  against emulated state, never delegated to TAIRiX: the guest can do to its
  own sandbox whatever RISC OS allowed, and reach the real machine only
  through the narrow, capability-checked, fail-closed bridges this plan
  defines (§6). A guest SWI is never a thin shim onto a TAIRiX syscall that
  would hand the guest ambient authority (§4).
- **`!Boot` is `autorun.inf`-class dangerous and is treated as such (§5,
  §19).** RISC OS filers run an application's `!Boot` merely on *seeing* it,
  to register its filetype and icon sprite; doing that unconditionally is the
  `autorun.inf` hole. TAIRiX never runs guest **code** from untrusted media on
  mere browsing: on removable media, viewing a filer window loads only the
  app's icon sprite as *data* and runs no `!Boot`. `!Run` runs only on an
  explicit, user-initiated launch, always inside the sandbox; whether and when
  an app's `!Boot` runs is governed by the media-provenance policy in §7 —
  never by an unconditional filer-style auto-boot.
- **Standalone and complete, no no-ops (§27, §2.1).** Every SWI, BASIC
  keyword, Obey command, and ARM instruction this plan lists is implemented
  as the real thing, tested against real RISC OS behaviour. A stubbed SWI
  that silently returns success is the §2.1 hack this plan forbids; an
  unimplemented-but-honest SWI returns the correct RISC OS "no such SWI"
  error (`&1E6`) so the guest can branch on it, exactly as real RISC OS does.

## 1. Read first (§15.18)

Before touching a covered area, read the anchor it builds on:

- **App bundles / loading / ABI** — `AGENTS.md` §9, §16.4, §16.5;
  `lib/abi/src/appinfo.rs` (the signed manifest, the declared
  MIME/file-type associations, `validate_bundle_layout`); `lib/appload`
  (the signed-bundle load gate); `userland/system/appmgr`; `plans/APPS.md`.
- **File metadata / RISC OS filetypes** — `lib/fsmeta` and
  `lib/fsmeta/src/preset/acorn.rs` (the `acorn.filetype`,
  `acorn.loadaddr`/`acorn.execaddr`, `acorn.attr`, `acorn.datestamp` keys
  already modelling RISC OS metadata); `drivers/filesystem/arxfs` and
  `drivers/filesystem/adfs`; `docs/src/filesystem/arxfs-spec.md`;
  `plans/ARXFS-METADATA.md`.
- **Launching / desktop / windows** — `plans/APPWIN.md` (the `lib/window`
  `WINDOW_ENDPOINT` app-window channel), `plans/DISPLAY.md`,
  `plans/SHELL.md` (command resolution), `plans/GUI-CONTROLS-DESIGN.md`
  (`lib/controls`), `plans/CAPABILITY_USE.md` (CU6 file picker).
- **Sandboxing untrusted execution** — `AGENTS.md` §19.5; `lib/sandbox`
  (the minimum-capability worker/serve seam); `docs/src/security/sandbox.md`.
- **Paths / references** — `plans/DRIVES.md`, `plans/ALIAS.md`, `lib/path`.

## 2. Prerequisite (OS-level): file-type run delegation

Every modern OS has a "run this data file with its registered handler"
facility; TAIRiX needs one anyway, and the emulator is its first real
consumer. This prerequisite is a **general** TAIRiX facility — it names no
RISC OS concept — and lands first, on its own, with its own tests, before any
emulator code.

### 2.1 The gap today

`AppInfo` already declares MIME / file-type associations (§16.5,
`appinfo.rs` `mime_count` / `mime_type_at`), and `lib/fsmeta` already decodes
a file's type from its metadata (`acorn.filetype`, and the foreign-FS
presets). What is missing is the **resolution + delegation** step: given a
target (a data file, or a directory the system should treat as a runnable
unit), decide *which installed handler bundle* runs it, and launch that
handler with a one-shot, user-mediated capability to the target — never
ambient authority (§4), never auto-run (§5).

### 2.2 `lib/openwith` — the association-resolution policy (shared, pure)

A new `no_std` + `alloc` crate `lib/openwith` (adds a §3 + `PLAN.md` row at
implementation start, per §6):

- Defines the **typed target kind** a launch acts on: a typed data file
  (identified by a resolved type token — a MIME string and/or a
  `lib/fsmeta` `acorn.filetype`), or a *bundle-like directory* the policy
  recognises as a runnable unit (the hook the emulator uses for `!App`
  directories, §5).
- Defines the **handler candidate model**: the ordered set of installed
  bundles whose signed `AppInfo` declares an association matching the
  target's type, plus the deterministic tie-break (manifest-declared
  association specificity, then system store before user store — the same
  determinism discipline as driver bind, §18.3). An unbroken tie is a
  packaging defect, never a coin-flip (§2.1).
- Is **spelling/policy only** — no I/O, no capability check, no launch. It
  is imported by the shell, the desktop/files app, and the delegation
  service so there is never a second association matcher (§2.2).
- Is fail-closed and bounded (§5.4, §24.4): an unrecognised type or an
  absent handler resolves to *no candidate*, never a guess.

### 2.3 The delegation entry point (service + syscall-free client seam)

Resolution is policy; *launching* is a capability-mediated action. The
delegate is exposed as a user-space path (mirroring §16.6's "call the API,
not a magic file"):

- A `lib/openwith` client seam issues an **open-target request** describing
  the target and (optionally) a preferred handler.
- The request is served by the app-launch owner. **Decision to fold into
  `userland/system/appmgr`** (it already owns bundle discovery + loading and
  holds `CAP_PROC_SPAWN`-class authority) rather than add a second service —
  a new sibling service would duplicate discovery (§2.2). This adds a
  *run-delegation* responsibility to `appmgr`; the §3 comment for `appmgr`
  is amended at implementation start.
- The handler is spawned through the existing signed-bundle load gate
  (`lib/appload`), and receives access to the target **only** as a one-shot
  descriptor handed across the process boundary — the CU6 picker pattern
  (`plans/CAPABILITY_USE.md`), never a broad filesystem capability. Every
  delegation decision (resolve, launch, deny) is logged with a stable event
  id (§19.4) and fails closed (§5.4).

### 2.4 No auto-run, ever (§5, §19)

Delegation runs a handler only on an **explicit user action** (typing a run
at the shell, double-clicking in the files app, an explicit "Open with…").
Merely *listing* or *browsing* a directory never triggers a launch. This is
the structural defence against the `!Boot`/`autorun.inf` class and is a
hard invariant of the prerequisite, independent of the emulator.

### 2.5 Prerequisite tests

- `lib/openwith`: association matching, ordering, tie-break determinism,
  fail-closed on unknown/absent handler; host unit tests + a fuzz harness on
  the (untrusted) type-token and association decode (§19.6).
- `appmgr` delegation: resolve→spawn→one-shot-descriptor happy path; deny
  paths (no handler, refused capability, malformed request) each a typed
  reply; the audit-log event ids; a QEMU vertical launching a trivial
  registered handler for a test filetype.
- An explicit test that **browsing never launches** (the anti-auto-run
  invariant).

## 3. The emulator bundle — crate shape and layering

The emulator is one `.app` bundle. Its code is **self-contained**: because
the emulator is the *sole consumer* of its ARM core, BASIC interpreter, SWI
dispatcher, Obey interpreter, and WIMP emulation, those live as
**host-testable modules / `lib` targets inside the emulator's own crate**,
exactly as a driver crate keeps its device logic (`AGENTS.md` §2.22), **not**
in `lib/*`. Only genuinely shared code (`lib/openwith`, and the already-shared
`lib/fsmeta` acorn keys) is in `lib/*` (§6, §2.2). If a second, unrelated
in-tree consumer ever appears for a component, it is hoisted to `lib/*` then —
not speculatively now (§2.3, §2.4).

Proposed source location: `userland/apps/riscos/` (an app crate under the
apps class). Its bundle `Run` binary is the emulator front-end; the guest
engine is its own host-tested `lib` target the `Run` binary links (the
driver-crate `Run` + `lib` pattern). No RISC OS module, sprite, ROM image, or
guest asset is compiled into the kernel or any tool (§16.5): guest data lives
on disk, loaded at runtime, treated as untrusted.

Internal module map (all inside the emulator crate):

- `arm/` — the ARM CPU interpreter (§4).
- `aif/` — the ARM executable-container decoders: AIF, RISC OS module
  format, and ELF-derived compiled-C output (§4.3).
- `basic/` — the BBC BASIC V/VI (ARM BASIC) tokeniser + interpreter (§5).
- `obey/` — the Obey command-script interpreter (§5).
- `swi/` — the SWI dispatcher and the `OS_`, `File_`/`OS_File`, `Font_`,
  `Wimp_`, … module implementations (§6).
- `kernel/` — the emulated RISC OS kernel state: RMA, dynamic areas,
  environment variables, error/`X` handling, the CAO/module chain (§6).
- `wimp/` — the WIMP-task emulation that maps guest windows to TAIRiX app
  windows (§8).
- `host/` — the thin bridge from emulated services to the sandboxed TAIRiX
  process capabilities (the *only* place guest state meets the real OS, §6.4).

## 4. ARM CPU emulation

RISC OS applications are BASIC (interpreted), ARM machine code, or C compiled
to ARM machine code. TAIRiX runs on x86_64 / aarch64 / riscv64 / wasm32, so
even the aarch64 host cannot run this code natively — 32-bit ARM *and* the
26-bit modes are a different architecture. The emulator therefore contains a
**portable ARM interpreter** written in Rust, host-testable and identical on
every TAIRiX target (§2.20, §2.21 — no `cfg(target_arch)`, it is pure logic).

### 4.1 Modes: 26-bit and 32-bit

RISC OS began 26-bit (ARMv2/ARMv3: PC and PSR packed into R15, `PSR` in the
top/bottom bits, 26-bit address space) and moved to 32-bit (ARMv4+, separate
CPSR/SPSR, 32-bit PC). This split forced the historic module rewrites the
issue mentions. The interpreter models **both**:

- 26-bit state: R15 = PC(bits 2..26) | flags | mode(bits 0..2); the
  address/PSR packing, `TEQP`/`MOVS pc,lr`-style mode changes, the 26-bit
  exception vectors.
- 32-bit state: full CPSR/SPSR, banked registers per mode (USR/FIQ/IRQ/
  SVC/ABT/UND, and later SYS), 32-bit PC.
- A per-image mode selection driven by the executable container / APCS
  flags (§4.3); a guest that assumes the wrong mode fails honestly, never
  silently mis-executes.

### 4.2 Instruction coverage

A **complete** ARM interpreter for the instruction sets RISC OS software
uses — not the subset one test app happens to hit (§27):

- ARMv2/v3/v4 integer: data-processing (all shifts/rotates, immediate and
  register), multiply/long-multiply, LDR/STR (all addressing modes, byte/
  word, later halfword/signed), LDM/STM (all four stack modes + `^`),
  B/BL, SWI, MRS/MSR, SWP, and the condition-code evaluation for every
  instruction.
- ARMv4T Thumb (later apps / C output): the Thumb encoding and BX
  interworking.
- FP: the historic FPA/FPE floating-point instruction set that older RISC
  OS BASIC/C relies on (`STFD`/`LDFD`/`FADD`/… via the FP coprocessor
  interface), and VFP where later software uses it — implemented as a
  software FP unit (correct rounding via `lib/*`-shared or in-crate soft
  float, never hand-rolled crypto-adjacent maths without tests, §2.12
  reasoning applied to correctness).
- Undefined/coprocessor instructions trap to the emulated
  undefined-instruction handler exactly as RISC OS would (the BASIC
  interpreter historically used this path), never a Rust panic (§2.9).

The interpreter is a decode+execute core with a clean instruction-decode
table (the §27 "right data structure" bar — a decode LUT, not a linear
`if`-chain). A JIT is explicitly **out of scope for v1** (correctness first,
§2.16 order of precedence); a later stage may add one behind the same
tested interface if measurement justifies it.

### 4.3 Executable containers

RISC OS code arrives in several shapes; the emulator decodes each into a
guest memory image + entry point, fail-closed and fuzzed (§19.5, §19.6):

- **Absolute / AIF** (ARM Image Format): the standard RISC OS application
  binary (`!RunImage` etc.) — the AIF header (self-relocation, zero-init,
  read-only/read-write sizes, entry, 26/32-bit flag, debug), decompression
  header where present.
- **RISC OS module format**: the module header (start/init/final/service/
  title/help/command-table/SWI-chunk/SWI-handler offsets) so
  module-provided SWIs and `*Commands` work.
- **Compiled C output**: Norcroft/`gcc` ARM output is AIF (or ELF for later
  toolchains). The ELF path reuses `lib/binfmt`'s ELF view for *structure*
  where applicable, but the emulator owns the ARM-specific loading; it does
  not pretend an ARM ELF is a TAIRiX-loadable image.

## 5. Obey files and BBC BASIC (ARM BASIC)

### 5.1 Obey

`!Boot` and `!Run` are usually **Obey** files (filetype `&FEB`), the RISC OS
command-script language. Getting an app to run *is* running its Obey script,
so the Obey interpreter is a first-class, fully-implemented component, not a
line-skipper:

- The `*command` set an app boot/run relies on: `Set`, `SetType`, `SetEval`,
  `SetMacro`, `Unset`, `IfThere`, `If … Then`, `Error`, `Obey`, `Run`,
  `WimpSlot`, `RMEnsure`/`RMLoad`/`RMRun`, `IconSprites`, `AddApp`, `Filer_*`,
  variable expansion (`<Var>`, `%0`–`%9`, `%*0`), and GS-string parsing
  (`OS_GSTrans`/`OS_GSRead` semantics) exactly per the PRMs.
- `*` commands map onto the emulated `OS_CLI` / module command table (§6),
  **not** onto TAIRiX's shell — an Obey `*Copy` copies inside the emulated
  filing view, gated by the sandbox's actual file capabilities, never a
  passthrough to TAIRiX (§0, §6.4).
- Unknown `*commands` produce the correct RISC OS "not recognised" error so
  scripts branch correctly (§27), never a silent skip.

### 5.2 BBC BASIC V/VI — ARM BASIC, not "a BASIC"

The issue is explicit: this is **BBC BASIC V/VI (ARM BASIC)** as documented
in the RISC OS BASIC PRMs — *not* a generic/PC BASIC. Prior attempts have
gone wrong by implementing the wrong dialect; this plan forbids that (§2.6).

- **Tokenised program format.** RISC OS BASIC stores programs as tokenised
  lines (line-number + length + tokenised bytes, the `&8D` line-number
  encoding, the two token tables incl. the `C6`/`C7`/`C8` extension sets).
  The emulator implements the exact tokeniser **and** detokeniser (LIST) so
  it loads real `.bbc`/`BASIC`-filetype (`&FFB`) programs byte-accurately.
  Plain-text programs are tokenised on load, as RISC OS does.
- **Interpreter semantics per the PRM.** The full language: integer/float/
  string variables and arrays, `FN`/`PROC` with `LOCAL`/`RETURN`, `DIM`
  (incl. `DIM x% n` block allocation), `!`/`?`/`$`/`|` indirection operators,
  `EVAL`, `OSCLI`, structured control (`REPEAT`/`UNTIL`, `WHILE`/`ENDWHILE`,
  `CASE`/`WHEN`/`OTHERWISE`, `IF`/`ELSE`/`ENDIF`), error handling
  (`ON ERROR`, `ERROR`, `REPORT`, `ERL`/`ERR`), and the ARM-specific pieces:
  `SYS "SWI name",…` (the BASIC gateway to SWIs — routed to the §6
  dispatcher), and `[ … ]` **inline assembler** (`OPT`, ARM mnemonics)
  assembling into guest memory and executed by the §4 interpreter.
- The interpreter runs the guest program; it does **not** compile BASIC to
  native TAIRiX code. `SYS`/`CALL`/`USR` cross into the ARM core and SWI
  layer, so BASIC and machine code interoperate exactly as on RISC OS.

## 6. SWI layer (RISC OS syscalls) — first-class, inside the emulator

RISC OS software calls the OS through **SWIs** (software interrupts): `SWI`
with a number, a name→number lookup (`OS_SWINumberFromString`), R0–R9 in/out,
V-flag + R0-error-pointer error convention, and the `X` (bit 17)
"don't-enter-error-handler" form. The emulator implements this **completely
inside itself** against emulated state (§0). SWIs are grouped by the RISC OS
module that provides them.

### 6.1 Dispatcher and error convention

- Number→handler dispatch over a table (§27 structure), the `X`/non-`X`
  split, the SWI-name registry, `OS_SWINumberToString`/`FromString`.
- The RISC OS error-block convention (V set, R0 → `{ errnum:u32; msg:cstr }`)
  and the standard error numbers, so a guest's `SYS` / `ON ERROR` behaves
  exactly as on hardware. An unimplemented SWI returns "SWI &xxxxxx not
  known" (`&1E6`) with V set — honest, branchable, never a fake success
  (§0, §27).

### 6.2 Core module SWIs to implement (v-by-v, all real)

Grouped; each is implemented fully within emulation bounds:

- **`OS_`** (the kernel): `OS_WriteC`/`WriteS`/`Write0`/`NewLine`,
  `OS_ReadC`, `OS_CLI` (→ emulated command table / Obey), `OS_Byte` and
  `OS_Word` (the sub-reason-coded VDU/keyboard/RTC calls actually used),
  `OS_File`/`OS_GBPB`/`OS_Find`/`OS_Args` (filing — see §6.3),
  `OS_ReadVarVal`/`OS_SetVarVal` (environment variables),
  `OS_GSTrans`/`OS_GSInit`/`OS_GSRead`, `OS_ReadUnsigned`/`OS_ReadModeVariable`,
  `OS_SpriteOp`, `OS_ReadMonotonicTime`, `OS_DynamicArea`, `OS_Module`
  (RMA/module chain), `OS_ChangeEnvironment`, `OS_GenerateError`,
  `OS_Claim`/`OS_Release` (emulated vectors), `OS_ServiceCall`, and the
  VDU/`OS_Plot` graphics primitives (→ the emulated screen, §8).
- **`File_`/`OS_File` filing**: load/save/stamp/set-type/set-attr/read-cat,
  operating over the emulated filing-system view whose backing is the
  sandboxed process's *actual* file capabilities (§6.4). RISC OS filename
  and path semantics (`$`, `&`, `%`, `\`, `.` as separator, `FS::disc.$.dir`)
  translated to/from TAIRiX paths (`lib/path`), and filetypes to/from the
  `lib/fsmeta` `acorn.*` keys (§9). No unchecked host access ever (§0).
- **`Font_`**: `Font_FindFont`/`Paint`/`ConverttoOS`/`ScanString`/… — a
  real font manager over the emulator's own glyph rendering (reusing
  `lib/font`/`lib/fontface` for rasterisation where the metrics allow),
  drawing into the emulated screen/window surface.
- **`Wimp_`**: `Wimp_Initialise`/`CreateWindow`/`CreateIcon`/`OpenWindow`/
  `CloseWindow`/`Poll`/`RedrawWindow`/`GetRectangle`/`DragBox`/
  `SendMessage`/`CreateMenu`/… — the WIMP, mapped to TAIRiX app windows
  (§8). This is the largest module and is staged (§10).
- Supporting modules as guest apps demand them: `Territory_`, `Message*`
  (MessageTrans), `ColourTrans`, `Draw_`, `Hourglass_`, `Sound_`,
  `Wimp_ReadSysInfo`, `TaskManager_`, added in specificity order driven by
  real test apps, each fully or honestly-absent (§27), never faked.

### 6.3 The emulated filing system

A guest sees RISC OS FS conventions (filetypes, load/exec words, RISC OS
paths, `*` filing commands). The emulator presents that view over a
**bounded, capability-gated** window onto real storage:

- Reads/writes go through the sandboxed process's file capabilities only
  (a one-shot descriptor to the launched app's own directory tree, plus any
  user-picker-granted files — §2.3, §5.4), never ambient access to `/`.
- RISC OS filetype ⇄ TAIRiX metadata is the shared `lib/fsmeta` acorn
  mapping (§9); load/exec words carry through losslessly (already modelled).
- `!App` directory structure (`!Run`, `!Boot`, `!Sprites`, `!Help`,
  `Messages`, `!RunImage`, sub-dirs) is understood natively.

### 6.4 The `host/` bridge — the only crossing point

Everything a guest SWI needs from the real world (file bytes, wall-clock
time, RNG, display surface, input events) is obtained *only* through the
emulator process's own capability-checked TAIRiX syscalls, funnelled through
`host/`. This is where "insecure RISC OS SWI" meets "capability-checked
TAIRiX": the SWI implements RISC OS semantics against emulated state, and
when it genuinely must touch the outside, it calls `host/`, which enforces
that the emulator only ever exercises authority it legitimately holds (§4,
§5.4). A guest cannot escalate past the emulator process's own grants.

### 6.5 Path conversion (RISC OS ⇄ TAIRiX)

RISC OS and TAIRiX spell paths incompatibly, and the two conventions even
disagree on which character separates directories. The emulator owns **one**
shared, bidirectional, host-tested conversion (a guest-specific concern, so it
lives in the emulator's filing module, not `lib/*` — §2.22), building on
`lib/path` for the TAIRiX side and `plans/DRIVES.md`/`plans/ALIAS.md` for
volume naming:

- **Directory separator swaps.** RISC OS uses `.` between path components, so
  `ADFS::4.$.!Application.!Boot` ⇄ TAIRiX `…/!Application/!Boot`. RISC OS
  reserves `.` and therefore represents an in-leaf dot (a foreign `name.ext`)
  with `/`. The two swap on conversion: RISC OS component separator `.` ⇄
  TAIRiX `/`, and RISC OS in-leaf `/` ⇄ TAIRiX in-leaf `.` (so RISC OS
  `README/txt` ⇄ TAIRiX `README.txt`). This swap is the classic RISC OS
  gotcha and is the first thing tested (a round-trip property test).
- **Roots and specials.** RISC OS `$` (filing-system root) maps to the root of
  the one directory tree the sandbox was actually granted (§6.3), *not* the
  TAIRiX `/` session root — the guest never sees outside its grant (§0, §5.4).
  The `FS::disc.` prefix (e.g. `ADFS::4.`, `SDFS::HardDisc4.`) is parsed and
  mapped to the corresponding TAIRiX volume/alias where one exists, or
  rejected fail-closed where it does not. RISC OS `&` (user root), `%`
  (library), `\` (previous), and `@` (current) resolve against the emulated
  environment, never a host path.
- **Filetype vs extension.** A RISC OS leaf carries no extension — its type is
  the `acorn.filetype` metadata (§9), not a suffix — so conversion never
  invents or strips a `.ext`; the type crosses through `lib/fsmeta` and the
  separator swap above is purely syntactic.
- Conversion is bounded and fail-closed (§24.4): an unmappable prefix, an
  over-long component, or a name that cannot round-trip is rejected with the
  correct RISC OS "bad name" error, never silently mangled.

## 7. Launch model, sandboxing, and `!Boot` policy

- **Every guest runs sandboxed (§19.5).** The ARM core, BASIC interpreter,
  and all guest-controlled parsing (AIF/module/tokenised-BASIC/sprite/font
  decode) run in a minimum-capability worker over `lib/sandbox`. A guest
  crash, illegal instruction, or malformed image is *contained* — a typed
  error to the front-end, the worker replaced, the event logged (§19.5) —
  never a crash of the emulator UI or the OS.
- **Running an app is a user action; passive browsing runs no guest _code_
  (§2.4).** An `!App`'s `!Run` executes only on an explicit launch
  (double-click / shell run / "Open with…") resolved through the delegation
  prerequisite (§2), which hands the emulator a one-shot descriptor to the
  target. Merely listing a directory never runs `!Run`. Whether the app's
  `!Boot` runs, and when, is decided by the media-provenance policy below —
  never by an unconditional filer-style auto-boot.
- **`!Boot` policy is keyed on media provenance, not a per-app dialog.** RISC
  OS filers run `!Boot` on merely *seeing* an app (to register its filetype
  and icon sprite); doing that unconditionally is the `autorun.inf` hole (§0,
  §19), yet approving 70 `!Boot`s one consent dialog at a time is equally
  unacceptable (the motivating case: a directory of ~70 `!App`s, each with a
  `!Boot`). The emulator therefore keys `!Boot` on **where the app lives**,
  obtained from the volume / hardware-tree removable flag (§18.1, §26.1),
  fail-closed — unknown provenance is treated as removable (the safer side):
  - **Fixed / local media — auto-`!Boot` allowed by default.** On
    non-removable local storage (the user's own installed system) an `!App`'s
    `!Boot` runs without a prompt: on an explicit launch, and — for the
    desktop filer view only — once when its window is first shown, so
    filetype/sprite registration works as on RISC OS. This removes the
    70-app dialog storm. `!Boot` runs in the same sandbox with the same
    bounded capabilities as `!Run`; there is no elevated boot path.
  - **Removable media — passive `!Boot` is never run; only the icon sprite
    loads.** Merely viewing a filer window of removable media runs *no*
    `!Boot` code. Instead, when the window is first shown (**desktop only —
    never the command line**), the emulator loads the app's **`!Sprites22`,
    falling back to `!Sprites`** (Acorn Sprite filetype `&FF9`) and uses its
    `IconSprite`-named sprite so the app at least shows its real icon. Sprite
    loading is pure *data* decode in the sprite-decode sandbox (§19.5) — no
    ARM / BASIC / Obey execution — so it is safe on untrusted media.
  - **Explicit launch from removable media runs `!Boot` then `!Run`.**
    Double-clicking an `!App` on removable media is an explicit user action,
    so it *does* run `!Boot` first, immediately followed by `!Run` (the RISC
    OS launch order), both sandboxed with the same bounded capabilities. The
    passive auto-boot ban applies only to the no-user-action filer-view case.
  - **Settable, tighten-only.** A user setting may *tighten* the policy (e.g.
    prompt-per-app, or deny auto-`!Boot` on local media too) but may never
    silently *loosen* the removable default. A refused or blocked `!Boot`
    fails closed and is reported honestly (§2.24 fail-loud), never silently —
    the app runs without its boot-time registration where it can, or does not
    run at all.

## 8. Windowed, fullscreen, and WIMP modes

RISC OS has two application shapes; the emulator honours both:

- **Single-tasking / "fullscreen" apps** (BASIC games, screen-mode apps
  that take over the display). Per the issue, these are **forced to start in
  a window** — the emulated RISC OS screen is rendered into a normal TAIRiX
  app window (`lib/window`, `plans/APPWIN.md`), with a **menu option to go
  fullscreen** (a real full-screen seat/display mode via `plans/DISPLAY.md`,
  returning to windowed on request). The emulated VDU/`OS_Plot`/sprite/mode
  calls render to that surface.
- **WIMP (desktop) apps.** A RISC OS desktop app calls `Wimp_Initialise` and
  polls. The emulator's WIMP maps each guest WIMP window to its own TAIRiX
  app window through the *same* `WINDOW_ENDPOINT` channel every native app
  uses (zero-copy shm surface, input routed back, park-when-idle §2.23) —
  so a WIMP app "launches like any other application in the OS", while the
  emulator reproduces the RISC OS look/behaviour (title bar, tools, menus
  via `Wimp_CreateMenu`) *inside* its window content. The emulator does not
  add a second compositor or a private display back-channel (§17.3, §10).
- Guest input (RISC OS mouse buttons Select/Menu/Adjust, key codes) is
  translated from `lib/input` events; guest redraw (`Wimp_RedrawWindow`
  rectangle loop) drives damage-tracked presents.

## 9. ARXFS and foreign-FS filetype integration

- ARXFS already stores RISC OS filetypes and load/exec words via the shared
  `lib/fsmeta` `acorn.*` keys, so an `!App` on any ARXFS volume is directly
  runnable through the §2 delegation with no per-FS special-casing.
- The ADFS/FileCore driver (`drivers/filesystem/adfs`) and the other
  foreign presets surface the same `acorn.*` metadata, so `!App`s on genuine
  Acorn media are equally runnable. The emulator reads types *only* through
  `lib/fsmeta`; it never re-implements a filetype decoder (§2.2).
- Copying an `!App` between filesystems preserves its RISC OS metadata
  because the copy path already carries the `acorn.*` keys (existing
  behaviour, `plans/ARXFS-METADATA.md`).

## 10. Staged deliverables (prerequisites first)

Each stage lands complete with its tests and docs (§7, §13); no stage begins
before its dependencies. `[ ]` = not started.

- **R0 — File-run delegation prerequisite (§2).** `[ ]` `lib/openwith` +
  the `appmgr` delegation responsibility + the anti-auto-run invariant.
  *No RISC OS code.* This is the OS hook the emulator (and every future
  handler app) needs. Depends on: nothing new. Tests per §2.5.
- **R1 — Emulator skeleton + ARM core (§3, §4).** `[ ]` The
  `userland/apps/riscos` bundle, the sandboxed guest worker (`lib/sandbox`),
  and the complete ARM interpreter (26- and 32-bit, integer + FP + Thumb),
  with an instruction-level conformance test suite (per-instruction vectors,
  condition codes, LDM/STM edge cases, mode switches, 26/32-bit PC/PSR).
  Depends on R0.
- **R2 — Containers + Obey (§4.3, §5.1).** `[ ]` AIF / module / ELF-ARM
  decoders (fuzzed, §19.6) and the full Obey interpreter, so a trivial
  absolute `!App` with a real `!Run` launches windowed, under the §7
  media-provenance `!Boot` policy (local auto-boot; removable runs `!Boot`
  then `!Run` only on explicit launch). Depends on R1.
- **R3 — BBC BASIC V/VI (§5.2).** `[ ]` Tokeniser/detokeniser + interpreter
  + inline assembler + `SYS` gateway. Tested against **real RISC OS BASIC
  programs** (a corpus of PRM examples and small real apps), byte-accurate
  tokenisation, and interpreter-behaviour vectors. Depends on R1.
- **R4 — Core `OS_` + filing SWIs (§6.1–§6.3).** `[ ]` The `OS_` kernel
  SWIs, `OS_File`/`GBPB`/`Find`/`Args`, environment variables, `OS_CLI` →
  command table, VDU/`OS_Plot` to the windowed screen, filetype ⇄ `fsmeta`,
  and the RISC OS ⇄ TAIRiX path conversion (§6.5, round-trip tested).
  Enough to run non-WIMP BASIC and ARM apps to completion. Depends on R2, R3.
- **R5 — Windowed screen + fullscreen toggle (§8).** `[ ]` The emulated
  screen rendered to a `lib/window` app window, the fullscreen menu option
  via `plans/DISPLAY.md`, VDU/sprite/mode rendering, input translation, and
  the desktop filer-view `IconSprite` loading (`!Sprites22`→`!Sprites`,
  data-only, §7) with the removable-media passive-`!Boot` ban. Depends on R4.
- **R6 — Font manager + graphics modules (§6.2).** `[ ]` `Font_*`,
  `ColourTrans`, `Draw_`, sprite ops — the drawing modules real apps need,
  over `lib/font`/`lib/raster`. Depends on R5.
- **R7 — WIMP (§6.2, §8).** `[ ]` `Wimp_*` mapped to TAIRiX app windows via
  `WINDOW_ENDPOINT`; a real RISC OS WIMP app runs on the TAIRiX desktop.
  Depends on R5 (R6 for text/graphics fidelity).
- **R8 — Drag-and-drop between TAIRiX and the guest (§0).** `[ ]` The RISC
  OS data-transfer protocol (`Message_DataSave`/`DataLoad`/`DataOpen`) bridged
  to a TAIRiX desktop drag-and-drop data-transfer channel, both directions,
  capability-mediated (one-shot descriptors, §2.3). Depends on R7 and the
  desktop DnD facility (`AGENTS.md` §10) existing; if that facility is not
  yet present, R8 is blocked and surfaced (§15.7), not stubbed.

## 11. Testing strategy (§7, §19.6)

- **Fidelity via real software.** The BASIC and SWI layers are validated
  against **real RISC OS programs** and PRM-documented behaviour, not
  self-invented expectations — the issue's explicit requirement. A curated,
  license-clean corpus (PRM examples; small apps; behaviour captured from
  documented RISC OS semantics and, where available, the RiscOS_371 sources)
  lives in the crate's test fixtures.
- **Instruction/decoder conformance.** Per-instruction ARM vectors and
  fuzzed container decoders (AIF/module/ELF/tokenised-BASIC/sprite) run under
  the §19.5 sandbox and enter the §19.6 regression corpus on any crash.
- **Containment.** Tests assert a malformed image / illegal instruction /
  runaway guest is contained (typed error, worker replaced, logged), never
  crashing the front-end or the OS.
- **Whole-project gate.** Every stage ends green on `cargo fmt --all`,
  `cargo xtask ci` (once), `cargo xtask fuzz --secs 5`, and the
  `tools/ci/soak.sh both --secs 20` smoke (§7), output quoted in the report.

## 12. Explicit non-goals (v1)

- **No native JIT** (§4.2) — correctness first; a measured JIT may follow.
- **No RISC OS ROM/HAL emulation.** The emulator provides SWIs and modules,
  not a bit-exact machine (no specific Acorn/Pi hardware, no real ROM image).
  It is an application-level RISC OS environment, not a board emulator.
- **No podule/expansion-card, real-network-stack, or real-sound-hardware
  emulation** beyond routing through TAIRiX's own capability-checked services
  where a guest module genuinely warrants it (added later, per real demand,
  never speculatively — §2.3).
- **No writing of C.** Every line here is Rust (§1, §15.11); the "compiled C"
  the emulator *runs* is a pre-existing ARM binary, decoded, never authored.
