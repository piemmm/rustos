# rustos-cmdres

Shared command-word resolution policy for RustOS (`lib/cmdres`,
`plans/APPS.md` §8–§9).

Every runnable program is an application bundle, `<name>.app`, whose entry
point is its `Run` binary. A typed command word resolves to bundle paths in
one fixed, deterministic order: the read-only, system-signed system app store
(`/System/Apps/`) first — so a user's `PATH` can never shadow a system
command — then the user's `PATH`, left to right, with the alias-aware `:`
split (`Home:/tools` is one entry). The shell's launch path and the `man`
command's bundle lookup both import this one definition; neither embeds a
second resolution policy.

## API

- `resolution_candidates(word, path_var)` — the ordered `Run`-binary
  spellings the shell's process host attempts for one command word. An
  explicit path (contains `/`) bypasses the search; a trailing `.app` names
  the bundle and runs its entry point; an empty word yields no candidates.
- `bundle_candidates(word, path_var)` — the same order as bundle-directory
  spellings, for consumers that read a bundle's *contents* rather than run
  it (`man` reads the first existing candidate's `Help/` tree, so the page
  shown always documents the program the shell would launch). An explicit
  path to a bare program names no bundle and yields the empty list — never
  a guessed sibling directory.
- `command_search_dirs(path_var)` — the ordered *directories* the bare-word
  search covers (the system app store, then the alias-aware `PATH`
  entries): the directory view of the same policy, which the shell's tab
  completion enumerates so it offers exactly the names launch would
  resolve.
- `search_roots(home)` — the app-store roots `man`'s recursive bundle
  search walks when the ordered candidates find nothing: the machine-wide
  `/Apps` (`rustos_abi::USER_APP_STORE`), then the user's own
  `<home>/Apps`. Spelling only — the bounded walk itself lives in the
  consumer; an unset or empty `home` contributes no per-user root. Launch
  is unaffected: the shell never consults these roots.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, never panics.
- Spelling only: no I/O, no permission checks, no authority. The host that
  consumes a candidate list owns the trusted load pipeline, and the kernel
  authorises every launch — a candidate list grants nothing.
- The store and bundle spellings come from `lib/abi`
  (`SYSTEM_APP_STORE`, `BUNDLE_SUFFIX`, `BundleEntry::Run`), the same
  definitions the kernel's program registry is drift-tested against, so the
  paths this crate spells and the paths the kernel registers cannot diverge.
- Empty `PATH` entries are skipped, never widened into a silent
  current-directory search.

## Stability

Tier: `experimental`.
