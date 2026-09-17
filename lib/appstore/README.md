# tairix-appstore

The one bounded walk of TAIRiX's installed program stores (`lib/appstore`,
`AGENTS.md` §16.2/§16.5/§16.8).

Three consumers need the same answer to "which bundles are installed, and
what does each one's own signed manifest say?":

- the program-library `rescan` (`userland/apps/applib`), which registers every
  bundle that declares a library folder,
- the file manager's "Open With…" table (`userland/apps/files`), which reads
  the file types each bundle claims,
- the desktop session's icon-bar identity index (`userland/gui/session`), which
  resolves the bundle a kernel-attested `AppIdentity` names.

Each carried its own copy of the walk, its own depth and entry bounds, and its
own manifest decode. This crate is that walk, defined once.

## API

- `StoreReader { list_dir, read_appinfo }` — the injected read seam.
  `Ok(None)` is absence (an unmounted store root, a directory that is not a
  bundle); `Err` is a real refusal.
- `MACHINE_ROOTS`, `user_roots(home)`, `store_roots(home)` — the store roots
  in the precedence a program name resolves against them: `/System/Commands`,
  `/System/Applications`, `/Apps`, then the account's own `Commands` and
  `Applications`.
- `walk(reader, roots, visit) -> Result<Scan, WalkError>` — offers each
  installed bundle to `visit` as a `Bundle { path, root, header, manifest }`
  and answers how many were accepted and how many skipped. The visitor's own
  `Verdict::Refused` counts as skipped.
- `manifest_path(bundle)`, `decode_manifest(bytes)` — the bundle-relative
  manifest path and the `APPINFO_WIRE_MAX`-bounded decode, for a consumer that
  reads one known bundle rather than walking.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, never panics.
- **No authority.** The crate performs no I/O and holds no capability: the
  consumer's own capability-checked filesystem access does, behind
  `StoreReader`. It verifies no signature either — a manifest read here is an
  unverified *claim*, and only the load gate (`lib/appload`) turns a claim into
  an attested identity. A consumer that draws an identity from a manifest must
  say which of the two it has.
- **Fixed precedence.** The system stores are read-only and system-signed and
  come first, so a user-writable store can never claim an identity a shipped
  bundle already declares. Each visited bundle carries the index of the root it
  was found under, so a consumer resolving a collision reads the precedence
  instead of re-deriving it from path prefixes.
- **Contained.** `MAX_WALK_DEPTH` bounds the descent into nested plain
  subdirectories; `MAX_WALK_ENTRIES` bounds the directory entries one scan
  examines across all roots. Both are fixed containment bounds on an untrusted
  tree (`AGENTS.md` §24.4), not capacities: exhausting either abandons the whole
  scan, so a caller acts on nothing rather than on a partial tree. A `.app`
  directory is a sealed unit and is never descended into.
- **Fail-closed per bundle.** A bundle whose manifest is absent, unreadable,
  over-long, or undecodable contributes nothing and is counted, so one broken
  bundle costs only itself.
- **Deterministic.** Listings are consumed in sorted order, so the same tree
  always yields the same sequence — which is what makes "the first bundle in
  store order owns an identifier" a rule rather than a race.

## Stability

Tier: `experimental`.
