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
- **Locale** — a BCP-47 language tag (e.g. `en-US`, `fr-FR`), plus the
  sentinel `default` (always en-US).

## Status

**In progress.** The maintainer decided the once-open design question in
favour of the **merge**: there is no separate `Documentation/` bundle entry —
the single internationalised `Help/` tree serves the CLI `man`, each
command's short `-h`/`-?` help, and any graphical help viewer (bundle-local
app documentation only; the OS source-tree docs under `docs/` are unrelated).
Landed: deliverable 1 (`BundleEntry::Help` replaced `Documentation` in place,
`AGENTS.md` §16.5 amended, C header regenerated), deliverable 2 (the
`lib/help` engine), deliverable 3 (the `man.app` command app, §7, with its
own six-locale `Help/` tree shipped on the read-only `/System` volume and
the `LANG` locale variable named in §5), and deliverable 4 (shell command
resolution over the `/System/Apps/` system app store, `AGENTS.md` §16.2
amended). The first fully-converged command app beyond `man` is **`ls`**
(deliverable 6): registered at `/System/Apps/ls.app/Run`, wired to the
real `fs_stat`/`fs_readdir` syscalls, shipping its six-locale `Help/`
tree (planted by `tools/mkimage` and the QEMU fixture), honouring the
§4 `-h`/`-?` short-help convention through `lib/help`, and emitting the
§12 `fs.hidden_entries_omitted` advisory record; the aarch64
session-ceiling vertical types `ls /System/Apps` end to end. Remaining:
deliverables 5–7 for the rest of the toolset (`cargo xtask help-lint`,
the OS `Help/` trees and `-h`/`-?` convention for the other command apps,
wider `stdinfo` adoption — `man`'s locale-fallback and `ls`'s omission
records are live), and the **charter-blocking** deliverable 8 —
self-contained on-disk bundles: today each command app's `Run` rxe is
baked into the kernel (`spawn_paths.rs`/`SPAWN_PROGRAMS`) with only `Help/`
on disk, which the amended §16.5 forbids (an app *is* its bundle
directory); the migration plants each app's signed `Run`/`AppInfo` on the
`/System/Apps` store, discovers the store from disk, verifies via `appmgr`,
and deletes the kernel registry. Help documents are authored **only** in each bundle's
on-disk `Help/` tree and read at runtime through the `lib/help` seam — no app
embeds its help, and the image builder plants the trees from data discovered
by `tools/syshelp`, never a hand-maintained per-bundle list (§6.1, `AGENTS.md`
§16.5).

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

`Help/` contains one subdirectory per locale, plus the mandatory `default`
sentinel:

```
top.app/Help/
├── default/           # ALWAYS en-US. The canonical source; MUST exist.
│   ├── top.md         # one Help document per command/topic
│   └── ...
├── fr-FR/
├── de-DE/
├── es-ES/
├── uk-UA/
└── it-IT/
```

- `default/` is the canonical help and is **always en-US**. It MUST exist for
  any bundle that ships `Help/`; a `Help/` tree without `default/` is a
  packaging defect and the loader/help engine fails closed (§5.4, §2.9).
- Each other directory is named by an exact BCP-47 tag and holds the same set
  of document file names as `default/`, translated.
- A locale directory MAY omit documents it has not translated yet; the help
  engine falls back per §5. It MUST NOT contain a document name absent from
  `default/` (there is nothing to fall back *from*, and it signals drift).
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
(§8) verifies that every switch the program accepts appears in `default/`'s
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
missing or malformed `LANG` selects the canonical `default/` documents: a
bad preference degrades to English, it never makes help unreadable.

Given a requested locale `ll-CC`, the help engine selects a document by the
first hit in this fixed, fail-safe chain:

1. `Help/ll-CC/<cmd>.md` — exact locale.
2. `Help/ll/<any-CC>/<cmd>.md` — same language, any region (deterministic:
   the lexicographically first matching directory, so the choice is stable).
3. `Help/default/<cmd>.md` — the en-US canonical document.

If even `default/<cmd>.md` is absent, the engine reports "no help for `<cmd>`"
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
  the one `lib/help` chain (exact tag → same language any region → `default/`
  en-US); a missing translation degrades to `default/`, never to fabricated or
  hardcoded text (§2.9).

## 7. The `man` command

`man` still exists, but it is RustOS'ised: it does **not** read the historical
troff/roff man format (RustOS ships none), it renders the `Help/` Markdown.

- `man` is itself a command app, `man.app`, in the system app store (§8) — it
  is not a shell builtin (it needs no shell-process state, `plans/SHELL.md`).
- `man <cmd>` resolves `<cmd>` through the **same** command-resolution path the
  shell uses (§9) to find the owning bundle, then renders that bundle's Help
  document for `<cmd>` in the active locale (§5) through `lib/help`.
- `man <cmd> <topic>` selects `Help/<locale>/<topic>.md` within `<cmd>`'s
  bundle, for bundles that ship more than one topic.
- `man` emits a `stdinfo` `omission`/`context` record (fd 3, §20) when it falls
  back to a non-requested locale or to `default`, so a tool or user knows the
  page was not shown in the requested language. This never affects `man`'s exit
  status or output correctness (§20).

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

- **Every OS-provided command app MUST ship a complete `Help/` tree**: a
  `default/` (en-US) document for every command it exposes, plus translations
  for the standing required locale set: `fr-FR`, `de-DE`, `es-ES`, `uk-UA`,
  `it-IT`. These documents MUST be generated and kept current; when an AI or a
  contributor changes a command's behaviour or switches, it updates the
  `default/` document and the translations in the same change (§2.8, §2.14,
  §2.18). Adding a language to the required set is data (a new locale
  directory), not new code.
- **No foul or derogatory content.** Help documents (all locales) MUST NOT
  contain profane, obscene, harassing, discriminatory, or otherwise derogatory
  language. This is a hard rule for generated and human-authored content alike.
- **Enforced in CI.** A `cargo xtask help-lint` check (run within
  `cargo xtask ci`, §7) fails closed when, for any OS command app: `default/`
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
   over `lib/vt` (widths from `lib/curses`). Unit tests, the `fuzz_help`
   harness registered in `cargo xtask fuzz` (§19.6), rustdoc,
   `lib/help/README.md`, `docs/src/lib/help.md`, and the §3 crate list are
   in place.
3. **`man.app`** — **done.** The RustOS `man` command app (§7):
   `userland/apps/man` resolves the word over `rustos_cmdres::
   bundle_candidates` (first existing bundle wins; `NotFound` moves on, any
   other refusal is final), loads and renders through `lib/help`, reads
   `LANG`/`PATH` from the inherited environment, pages on a
   geometry-attested console (space/return/`q`, echo suppressed) and
   streams otherwise, and emits the §7 `stdinfo` `context` record
   (`help.locale_fallback`) on a locale fallback. Registered as
   `/System/Apps/man.app/Run` (manifest: console pair + `CAP_FS_ACCESS`);
   its own six-locale `Help/` tree is authored on disk in the bundle and
   read at runtime through the `BundleStore` seam (no help embedded in the
   binary, §6.1) — the tree is discovered by `tools/syshelp` and planted on
   the read-only `/System` volume by `tools/mkimage` and the QEMU image
   fixture; the `session_ceiling` vertical types `man man` end to end.
4. **Shell command resolution** — **done** (except the per-app `-h`/`-?`
   convention, which lands with each app's `Help/` tree, §4/§8.1):
   system-app-store-then-`PATH` resolution (§8) and `.app`-suffix invocation
   (§9) are live. The store/bundle spellings live once in `lib/abi`
   (`SYSTEM_APP_STORE`/`BUNDLE_SUFFIX`); every OS command app is registered
   as `/System/Apps/{elsh,ls,man,ps,sysinfo,top,users}.app/Run`
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
5. **`cargo xtask help-lint`** — the §8.1 content/completeness/switch-drift
   check, wired into `cargo xtask ci` (§7).
6. **`Help/` trees for the existing command apps** — **`ls` done** (its
   six-locale tree is authored on disk in the bundle, discovered by
   `tools/syshelp`, planted at `/System/Apps/ls.app/Help/`, and served — at
   runtime through the `HelpSource` seam, never embedded in the binary
   (§6.1) — for its §4 `-h`/`-?` short help); remaining: `ps`, `top`, `cat`,
   `cp`, `mv`, `rm`, `chmod`, `chown`, `mount`, `getcap`, `setcap`,
   `useradd`, `groupadd`, `elsh`, `sysinfo`, `terminal`, … in `default/`
   plus the required locales (§8.1). Each new tree ships by dropping its
   `Help/` files under the bundle — `tools/syshelp` rediscovers them, and no
   image-builder list is edited (§6.1).

7. **`stdinfo` adoption in command apps (§12)** — emit the appropriate
   `StdInfoRecord` (via the `lib/rt` wrapper) wherever a command omits,
   summarises, or adds non-obvious context to `stdout`. Live: `man`'s
   locale-fallback record (§7) and `ls`'s `fs.hidden_entries_omitted`
   omission record (the `AGENTS.md` §20.1 canonical example);
   advisory-only, never changing exit status.

8. **Self-contained on-disk bundles — retire the kernel-baked spawn
   registry (charter-blocking, in progress; §16.5 self-containment and
   §16.2 services-are-apps amended).** Every program's `Run` rxe — the
   command apps *and* the `login`/`devmgr`/`sysinfod` services (a service
   is an app, §16.2) — is today compiled into the kernel image and
   dispatched by a byte-exact in-kernel path lookup
   (`kernel/rustos-kernel/src/spawn_paths.rs` + `spawn_layout.rs`'s
   `include!`-ed `*_rxe.rs` and `SPAWN_PROGRAMS`), while only each bundle's
   `Help/` tree is planted on disk — so `/System/Apps/ls.app/` shows only
   `Help/`, no `Run`/`AppInfo` exist on the volume, and the `appmgr`
   signature + capability + interface-hash path is bypassed. The amended
   §16.5/§16.2 forbid this. The maintainer decided (2026-07-04) against any
   staged compatibility: the full correct end state lands in dependency
   order — the binding increment list, with per-increment status, lives in
   `PLAN.md` ("Self-contained bundles"): (1) the canonical bundle
   content-hash framing in `lib/abi`; (2) per-crate `AppInfo.toml` manifest
   sources + the discovery walk (never a per-bundle list) + the shared
   composer signing under the dedicated `SYSTEM_APP_SIGNING_SEED`; (3)
   `tools/mkimage` and the QEMU fixture plant each discovered bundle's
   signed `AppInfo` + `Run` beside its `Help/` (command apps under
   `/System/Apps/`, services under `/System/Services/<name>.app/`); (4)
   the verification engine hoisted from `appmgr` into a `lib/*` crate and
   `spawn` loading + verifying store bundles from the mounted volume; (5)
   the x86_64/riscv64 storage floor, then deletion of `SPAWN_PROGRAMS`,
   the `*_rxe.rs` `include!`s (all but PID 1 `init`), `spawn_paths.rs`,
   and `program_manifests.rs` (§2.14). All prior deliverables' references
   to `/System/Apps/<cmd>.app/Run` being served from `spawn_paths.rs` are
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
