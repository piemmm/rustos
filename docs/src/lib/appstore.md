# `tairix-appstore` — the installed-bundle walk

`lib/appstore` (`tairix-appstore`) is the one bounded walk of TAIRiX's program
stores (`AGENTS.md` §16.2, §16.5, §16.8). Three consumers need the same answer
to "which bundles are installed, and what does each one's own signed manifest
say?":

- the program-library `rescan` (`userland/apps/applib`), which registers every
  bundle that declares a library folder,
- the file manager's "Open With…" table (`userland/apps/files`), which reads
  the file types each bundle claims,
- the [desktop session](../desktop/session.md)'s icon-bar identity index, which
  resolves the bundle a kernel-attested `AppIdentity` names.

Each carried its own copy of the walk, its own depth and entry bounds, and its
own manifest decode. This crate is that walk, defined once (§2.2).

## What it does

`walk(reader, roots, visit)` offers each installed bundle to `visit` as a
`Bundle { path, root, header, manifest }` and answers a `Scan { accepted,
skipped }`:

- **Breadth-first, sorted.** Listings are consumed in name order, so the same
  tree always yields the same sequence — which is what makes "the first bundle
  in store order owns an identifier" a rule rather than a race.
- **A `.app` is a sealed unit.** The walk never descends into one, so a bundle
  can never contain another; nested *plain* subdirectories are descended, so a
  store may be organised (`/Apps/games/chess.app`).
- **Contained.** `MAX_WALK_DEPTH` bounds the descent and `MAX_WALK_ENTRIES`
  the directory entries one scan examines across all roots. Both are fixed
  containment bounds on an untrusted tree (§24.4), not capacities: exhausting
  either returns `WalkError::TreeTooLarge` and abandons the whole scan, so a
  caller acts on nothing rather than on a partial tree. A directory that exists
  but cannot be listed is `WalkError::Listing` for the same reason.
- **Fail-closed per bundle.** A bundle whose manifest is absent, unreadable,
  over-long, or undecodable contributes nothing and is counted among
  `Scan::skipped`, so one broken bundle costs only itself. The visitor's own
  `Verdict::Refused` counts the same way.

`store_roots(home)` spells the roots in the precedence a program name resolves
against them (§16.8): `/System/Commands`, `/System/Applications`, `/Apps`, then
the account's own `Commands` and `Applications`. `MACHINE_ROOTS` is the
machine-wide prefix on its own, for a consumer with no account in hand, and
`user_roots(home)` the account's pair — empty for a session with no usable
`HOME`, so every machine-wide store is still walked.

`manifest_path(bundle)` and `decode_manifest(bytes)` are the same
bundle-relative path and the same `APPINFO_WIRE_MAX`-bounded decode the walk
uses, for a consumer that reads one *known* bundle rather than walking.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, never panics (§2.9).
- **No authority.** Reading is injected through the `StoreReader` seam
  (`list_dir` + `read_appinfo`, `Ok(None)` for absence and `Err` for a real
  refusal), so the crate performs no I/O and holds no capability: the
  consumer's own capability-checked filesystem access does, under its own
  kernel-attested identity. A running system backs the seam with the secured
  VFS; tests back it with an in-memory tree.
- **It verifies no signature.** A manifest read here is an unverified *claim*.
  Only the [load gate](./appload.md) turns a claim into an attested identity,
  and a consumer that draws an identity from a manifest must say which of the
  two it has — the session's icon bar matches its claims against what the
  kernel attested rather than believing them (§23.1).
- **Fixed precedence, reported not re-derived.** Each visited bundle carries
  the index of the root it was found under. The walk is breadth-first across
  every root at once, so a nested bundle in a system store is *seen* after a
  top-level one in a user store; a consumer resolving a collision therefore
  reads the precedence instead of guessing from visit order or re-parsing path
  prefixes.

## Stability

Tier: `experimental` (`lib/appstore/README.md`).
