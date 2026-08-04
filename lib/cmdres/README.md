# tairix-cmdres

Shared command-word resolution policy for TAIRiX (`lib/cmdres`,
`plans/APPS.md` §8–§9, `AGENTS.md` §16.8).

Every runnable program is an application bundle, `<name>.app`, whose entry
point is its `Run` binary. A typed command word resolves to bundle paths in
one fixed, deterministic order — a **non-overridable four-directory prefix**,
then the user's `PATH`:

1. `/System/Commands` — the read-only, system-signed system command store.
2. `/System/Applications` — the system application store, so a desktop
   application is typeable by name too.
3. `<home>/Commands` — the user's own command store.
4. `<home>/Applications` — the user's own application store.
5. the user's `PATH`, left to right, with the alias-aware `:` split
   (`Home:/tools` is one entry).

The prefix is built from the shared store definitions, never read from the
environment, so a session with no `PATH` and no `HOME` still resolves every
system program, and no exported value can reorder it, remove it, or shadow a
system command. Both system stores precede every user-writable directory, and
a `PATH` entry repeating a prefix directory is dropped rather than searched
twice. The shell's launch path, its tab completion, and the `man` command's
bundle lookup all import this one definition; none embeds a second resolution
policy.

## API

- `CommandEnv { home, path_var }` — the inherited session values the order
  reads, as a named pair so a call site cannot transpose them.
- `resolution_candidates(word, env)` — the ordered `Run`-binary spellings the
  shell's process host attempts for one command word. An explicit path
  (contains `/`) bypasses the search; a trailing `.app` names the bundle and
  runs its entry point; an empty word yields no candidates.
- `bundle_candidates(word, env)` — the same order as bundle-directory
  spellings, for consumers that read a bundle's *contents* rather than run it
  (`man` and a program's own `-h` read the first existing candidate's `Help/`
  tree, so the page shown always documents the program the shell would
  launch). An explicit path to a bare program names no bundle and yields the
  empty list — never a guessed sibling directory.
- `command_search_dirs(env)` — the ordered *directories* the bare-word search
  covers: the directory view of the same policy, which the shell's tab
  completion enumerates so it offers exactly the names launch would resolve.
- `search_roots(home)` — the store roots `man`'s recursive bundle search walks
  when the ordered candidates find nothing: the machine-wide `/Apps`
  (`tairix_abi::INSTALLED_APP_STORE`), then the user's own two stores. The
  flat system stores are absent by design — the ordered candidates already
  cover them and there is nothing nested to walk. Spelling only: the bounded
  walk itself lives in the consumer, and an unset or empty `home` contributes
  no per-user root. Launch is unaffected; the shell never consults these
  roots.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, never panics.
- Spelling only: no I/O, no permission checks, no authority. The host that
  consumes a candidate list owns the trusted load pipeline, and the kernel
  authorises every launch — a candidate list grants nothing. A store
  directory that does not exist simply contributes candidates nothing is
  found under; existence is the host's I/O question.
- The store and bundle spellings come from `lib/abi`
  (`SYSTEM_COMMAND_STORE`, `SYSTEM_APPLICATION_STORE`,
  `HOME_COMMAND_STORE_DIR`, `HOME_APPLICATION_STORE_DIR`,
  `INSTALLED_APP_STORE`, `BUNDLE_SUFFIX`, `BundleEntry::Run`), the same
  definitions the kernel's program registry is drift-tested against, so the
  paths this crate spells and the paths the kernel registers cannot diverge.
- Empty `PATH` entries are skipped, never widened into a silent
  current-directory search.

## Stability

Tier: `experimental`.
