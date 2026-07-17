# SMARTRAM.md - Opportunistic reclaimable memory services

Status: in progress — SMART1 (the reclaimable-memory classification and
accounting model, `kernel/mem::reclaim`: the complete class taxonomy with
deterministic reclaim priorities, the owner model for the owners the
kernel already has, rebuild-cost/sensitivity/invalidation-source/
reclaim-rule modelling, the bounded per-entry-metadata validation bound,
the fail-closed classification gate with typed refusals, and per-class
checked accounting), the clean and rebuildable filesystem cache
(section 6.1, `kernel/core::fs::CachedFs`, wrapping every registered
volume driver and classified through that gate at construction),
SMART2 (the VM pressure model and reclaim ordering,
`kernel/mem::pressure`: the shared five-band gauge with hysteresis over
the frame allocator, the reserve floor, per-band per-class shrink
targets, the `ramzip` handoff gate, deterministic escalation, and the
`CachedFs` per-operation enforcement), SMART3 (the ARXFS transform
cache: the driver's injected `ClusterCache` seam and the kernel's
classified, budgeted, pressure-governed, zeroing
`TransformClusterCache`, installed on both boot volumes), SMART4
(the semantic application-launch cache,
`kernel/core::launch_cache::LaunchCache`: the classified, budgeted,
pressure-governed retention of the load gate's accepted `LoadedApp` for
immutable system-store bundles), SMART9 (observability through
existing diagnostics: the split payload/metadata ledger with
pressure-shrink/teardown/failure counters, the pressure gauge's
per-band transition counters, and the `kernel/mem` `reclaim_audit`
events; `plans/STRESSTEST.md` ST1 has since exported them through the
capability-gated System Information queries), SMART10 (the cross-cache integration,
thrash, and benchmark-evidence suites over one shared gauge), and
SMART11 (the whole-disk block-level LRU cache,
`kernel/rustos-kernel::block_cache::BlockCache`: the classified,
budgeted, pressure-governed, zeroing per-block cache the boot path
installs under the block-sharing layer; see `PLAN.md` §SMARTRAM for
the done-state summaries) are implemented; SMART5–SMART8
(desktop/UI, reliability-assist, background-validation, and predictive
caches) are **shelved — not added**; the remaining classes are staged
below  
Target: RustOS  
Primary code areas: `kernel/mem`, `kernel/core`, `kernel/sched`, `lib/log`, existing filesystem drivers, existing desktop/session crates, and existing `lib/abi` diagnostics only if a current caller requires them  
Secondary code areas: `drivers/filesystem/arxfs`, `userland/system/appmgr`, `userland/shell/elsh`, `userland/gui/wm`, `userland/gui/taskbar`, `userland/gui/session`, `lib/appload`, `lib/cmdres`, `lib/raster`, `lib/svg`, `lib/font`, `lib/icon`, `lib/theme`, and `lib/path`  
Repository placement: `plans/SMARTRAM.md`, unless the repository layout in `AGENTS.md` is updated to permit another location

This document defines the staged RustOS plan for using otherwise idle RAM as a
bounded, reclaimable set of memory services rather than as only a disk block
cache. It is a design, invariant, staging, and acceptance document. It is not a
changelog and not a merge-ready implementation.

If this document conflicts with `AGENTS.md`, `AGENTS.md` wins. If this document
conflicts with `plans/SWAPSWAPSWAP.md` on the encrypted compressed anonymous
memory tier, `plans/SWAPSWAPSWAP.md` owns that tier and this document must be
corrected to reference it rather than duplicate it. If this document conflicts
with the repository's actual memory manager, scheduler, filesystem, desktop,
ABI, or documentation state, the implementing agent must surface the mismatch
before implementing a guess.

---

## 1. Source of truth

Before touching code, the implementing agent must read and reconcile:

1. `AGENTS.md`.
2. `PLAN.md`.
3. `plans/SWAPSWAPSWAP.md`.
4. `plans/ALIAS.md`.
5. `plans/DRIVES.md`.
6. `plans/ARXFS-METADATA.md` (extended-attribute model, `lib/fsmeta`, and the
   namespace-scoped capability rules any metadata cache must preserve).
7. `docs/src/architecture/memory.md`.
8. `docs/src/architecture/security.md`.
9. `docs/src/filesystem/drives.md`.
10. `docs/src/filesystem/arxfs-spec.md`.
11. `kernel/mem`.
12. `kernel/core`.
13. `kernel/sched`.
14. `kernel/sec`.
15. Existing VM pressure, page-reclaim, OOM, page-cache, filesystem-cache,
    scheduler-pressure, logging, and diagnostics code.
16. `drivers/filesystem/arxfs` and any existing foreign filesystem cache code.
17. `userland/system/appmgr`, `userland/shell/elsh`, and existing command or
    application-resolution helpers.
18. `userland/gui/wm`, `userland/gui/taskbar`, and `userland/gui/session`.
19. Existing shared libraries that already own parsing, rendering, path,
    application-load, command-resolution, compression, crypto, logging, theme,
    icon, font, raster, or metadata behaviour.

RustOS rules that matter especially for this work:

- The implementation remains Rust-only, with no C, C++, hand-written headers,
  build glue in another language, or new assembly.
- Security and correctness are the floor; performance is a first-class goal
  inside that floor.
- Shared logic and constants are defined once and imported by every consumer.
- Public ABI, syscalls, capabilities, driver traits, IPC methods, and userland
  services are not added unless a current in-tree caller requires them and the
  ABI, docs, tests, and roadmap changes land in the same fully-gated change.
- Production paths do not use `unwrap()`, `expect()`, `panic!()`, `todo!()`,
  ignored tests, sleep loops, retry loops, global mutable statics, or
  commented-out tests.
- Any `unsafe` required by memory-management code has a `// SAFETY:` block, is
  encapsulated behind a safe API, and is covered by a unit test or model check.
- Every implementation stage lands with rustdoc, mdBook documentation, unit
  tests, integration tests where applicable, fuzz/property tests where
  applicable, benchmark evidence where policy is performance-based, and the
  whole-project validation gate required by `AGENTS.md`.
- Implementations are tested with benchmarks (section 14), and every security
  feature this plan requires - fail-closed refusal, capability filtering,
  cross-principal isolation, plaintext zeroization, invalidation on policy or
  generation change - is checked with proper tests: dedicated adversarial and
  negative unit tests, plus fuzz or property targets where untrusted input or
  a decoder is involved. A security feature that lands without its test is
  incomplete.
- Planning files describe current design, invariants, and remaining staged work;
  they do not keep a changelog.

---

## 2. Goal

RustOS shall treat spare RAM as a general, bounded, opportunistic, reclaimable
memory-services pool. Disk and filesystem cache remain important, but the
system should also use idle RAM to avoid repeated computation, verification,
decompression, rendering, parsing, lookup, recovery, and cold-page restoration
work.

The target model is:

```text
free RAM above reserves
  -> clean/rebuildable filesystem and metadata cache
  -> transformation cache
     - verified
     - decrypted where already permitted by the owning filesystem path
     - decompressed
     - parsed
  -> semantic app/runtime cache
  -> desktop/UI cache
  -> reliability and recovery assist cache
  -> background validation working cache
  -> predictive prefetch cache
  -> SWAPSWAPSWAP ramzip for cold anonymous pages under pressure
  -> optional lower-tier encrypted swap only if approved by SWAPSWAPSWAP
```

The internal working name in this document is `smartram`. The production name
may differ, but the semantics must remain clear: this is a policy and accounting
layer for reclaimable RAM-resident objects, not a persistent store, not a disk
swap subsystem, not a way to bypass filesystem permissions, and not a public
user-tuning surface.

---

## 3. Relationship to `SWAPSWAPSWAP.md`

`plans/SWAPSWAPSWAP.md` owns the encrypted compressed anonymous-memory tier,
including `ramzip`, page eligibility, compression-before-encryption,
authenticated encrypted blobs, decompression reserves, page-table compressed
entry state, thrash detection, background warm-up, optional lower-tier encrypted
swap, and the lower-tier storage identity rules.

This document owns everything around that tier:

- reclaimable filesystem, metadata, transform, semantic, UI, reliability,
  validation, and predictive caches;
- the common classification and accounting model for non-`ramzip` reclaimable
  memory;
- pressure ordering between cheap disposable caches, clean cache, transform
  cache, semantic cache, and `ramzip`;
- cache invalidation, privacy, ownership, and diagnostics for reclaimable
  memory services;
- userland-facing cooperation only where a complete in-tree caller and tested
  interface exist.

The pressure-band vocabulary (normal, mild, moderate, severe, critical) is the
one VM pressure model shared with `plans/SWAPSWAPSWAP.md`, never a parallel
vocabulary. Where the two documents describe the same band, the band's meaning
is identical; `SWAPSWAPSWAP.md` section 6 already places clean file cache and
disposable-cache reclaim at mild pressure, and this document follows it.

Required ordering with `SWAPSWAPSWAP.md`:

```text
normal:
  allow bounded opportunistic smartram caches

mild pressure:
  stop speculative growth; reclaim disposable/rebuildable smartram caches and
  begin reclaiming clean file cache, matching SWAPSWAPSWAP section 6

moderate pressure:
  finish reclaiming clean file cache and reclaim transform caches before
  anonymous pages are compressed into ramzip, unless the existing memory
  manager proves a stricter order

severe pressure:
  let ramzip grow only under the reserve, cap, and fail-closed rules owned by
  SWAPSWAPSWAP; smartram keeps only objects justified by latency or recovery
  policy and still releases them on request

critical pressure:
  stop all speculative work; escalate through the memory-pressure policy owned
  by the VM and SWAPSWAPSWAP
```

`smartram` must not depend on `ramzip` internals. It may consume stable internal
VM pressure states and accounting summaries. If those states do not yet exist,
the stage that needs them must implement the complete in-tree interface with
callers, tests, docs, and validation.

---

## 4. Non-goals

This work does not implement:

- the encrypted compressed anonymous-page tier described by
  `plans/SWAPSWAPSWAP.md`;
- disk swap, ARXFS swap, hibernation, or persistent crash dumps;
- a Linux-compatible `/proc`, `/sys`, `free`, `swapon`, `zramctl`, or `zswap`
  surface;
- a public user tuning UI;
- speculative public ABI, syscalls, capabilities, driver traits, or IPC methods;
- a second parser, renderer, path resolver, app loader, command resolver,
  compression layer, crypto path, or diagnostics system where an existing
  shared crate already owns that behaviour;
- caching that weakens capability checks, filesystem permissions, MAC policy,
  signature checks, encryption, authentication, or fail-closed behaviour;
- a guarantee that cached plaintext protects against a fully compromised
  running kernel.

---

## 5. Design summary

`smartram` classifies reclaimable memory by what it represents, who owns it,
how expensive it is to rebuild, how sensitive it is, and how quickly it must be
released under pressure.

The core object model is conceptual; concrete type names must follow the
repository's existing naming style:

```text
reclaimable object:
  class:
    disposable_ui
    clean_file_cache
    fs_metadata
    transform_cache
    semantic_app_cache
    runtime_cache
    reliability_assist
    background_validation
    predictive_prefetch
  owner:
    kernel subsystem | filesystem instance | userland service | task/address space
  rebuild cost:
    cheap | moderate | expensive | recovery_critical
  sensitivity:
    public | user_data | system_data | secret_derived | credential_or_key_forbidden
  state source:
    canonical object identity + generation/checksum/version
  accounting:
    physical bytes + metadata bytes + logical bytes represented
  reclaim rule:
    drop | shrink | flush_then_drop | notify_owner | preserve_until_severe
```

Objects whose type is unknown, whose owner cannot be charged, whose invalidation
source is unclear, or whose data may contain credentials, cryptographic keys, or
capability-critical material are ineligible. Unknown fails closed.

`smartram` is not a single global bag of memory. It is a set of owner-charged,
classed, bounded caches that the memory manager can reclaim deterministically.
The implementation must avoid a single overgrown cache externalising its cost to
the rest of the system.

---

## 6. Cache classes

### 6.1 Clean and rebuildable filesystem cache

Clean file data, directory entries, path lookup results, stat metadata,
extended metadata, ACL/security-label lookups, content hashes, signature
verification results, and type-detection results may use spare RAM when they
are tied to a canonical storage identity and generation.

Current state: implemented as `kernel/core::fs::CachedFs` over the
`kernel/mem::reclaim` classification/budget/accounting model — per-volume,
charged to the volume's `ReclaimOwner` through the fail-closed
classification gate at construction, write-through, below the secured VFS
(no authorisation bypass), LRU-bounded
with hysteresis, zeroing every reclaimed buffer, invalidated precisely by
the volume's single writer (the shared registered driver). Within one boot
the driver instance is the volume generation (a mount starts an empty
cache); removable-media generation tokens arrive with the storage
subsystem that introduces removable volumes. Extended-attribute and
content-hash caching land with their consumers.

Rules:

- clean file cache is dropped before cold anonymous pages are compressed into
  `ramzip`, and its reclaim begins at mild pressure, matching
  `plans/SWAPSWAPSWAP.md` section 6;
- filesystem permissions and capabilities are checked at open/resolve/use time,
  never bypassed because metadata is cached;
- extended-attribute metadata follows the `plans/ARXFS-METADATA.md` model:
  cached attribute values and cached `list` results preserve the
  namespace-scoped capability filtering (a listing cached for a caller who may
  read `system.*` keys is never served to a caller who may not), and the one
  key grammar and preset registry remain in the shared `lib/fsmeta` crate,
  never re-derived by a cache;
- cache keys use durable volume/root identity, inode or object identity, and a
  generation or equivalent invalidation token;
- removable or replaced media invalidates by generation, not by discovery-order
  device names;
- dirty data is not disposable cache and is handled by the filesystem's write,
  journal, COW, or flush policy.

### 6.2 Transformation cache

Current state: the ARXFS decompressed-cluster cache is implemented (the
SMART3 stage entry below and `docs/src/architecture/memory.md` section
7i): the driver's injected `ClusterCache` seam plus the kernel's
classified, budgeted, pressure-governed, zeroing
`TransformClusterCache`, installed on both boot volumes. The remaining
transform families listed here land with the stages that build their
consumers.

Transformation cache stores expensive intermediate forms of authorized data:

- verified filesystem records;
- decrypted blocks or records after the owning filesystem has authorized the
  access path;
- decompressed filesystem records;
- parsed filesystem metadata;
- decoded or normalized foreign-filesystem metadata;
- verified executable or bundle payload records;
- prevalidated RXE, manifest, or interface-hash state through existing app-load
  code.

Rules:

- transformation cache never stores cryptographic keys, capability tokens,
  credentials, or kernel secret material;
- decrypted user data is accounted and protected as user data, not as public
  metadata;
- authenticated or verified state is invalidated when the underlying bytes,
  key epoch, mount generation, policy epoch, or signature epoch changes;
- temporary plaintext buffers used by transformations are zeroed before reuse or
  release when they may contain sensitive data;
- cached plaintext derived from encrypted storage (decrypted blocks, records,
  or metadata) is zeroed when the cache entry is reclaimed, invalidated, or its
  owner is torn down, so reclaim never leaves stale plaintext in reusable
  frames;
- transform cache is reclaimable and must not be required for correctness.

### 6.3 Semantic application and runtime cache

Current state: implemented for every part with a current in-tree
consumer (the SMART4 stage entry below and
`docs/src/architecture/memory.md` section 7j). The kernel spawn path's
semantic launch cache (`kernel/core::launch_cache::LaunchCache`, held by
the `AppStore`) retains the shared load gate's accepted `LoadedApp` —
parsed signed manifest, content-hash and interface-hash verdicts,
dynamic-loader policy decisions, and the validated `rxe` image — for
bundles on the immutable read-only system stores, once per boot.
Command-resolution *output* caching and a separate RXE
relocation-preparation cache are deliberately **not** built (scope
decisions recorded in the SMART4 entry below).

Semantic cache stores expensive application-launch and command-resolution
results:

- parsed `AppInfo` manifests;
- signed bundle validation summaries;
- content-hash and interface-hash verification summaries;
- dynamic-loader policy decisions;
- command-word resolution candidate lists from the shared command resolver;
- RXE validation state and relocation/linking preparation where the existing
  loader model supports it;
- recently used application resource maps.

Rules:

- app-load and command-resolution logic remains in the existing shared crates;
  `smartram` only caches their outputs with correct invalidation;
- capability intersection is recomputed or revalidated whenever the caller,
  executable manifest, user, policy epoch, or capability authority changes;
- cache entries are scoped to the authority that produced them when the result
  contains user- or capability-dependent data;
- invalidation is tied to bundle identity, content hash, signature epoch,
  policy epoch, and storage generation.

### 6.4 Desktop and UI cache

Desktop cache stores UI data that is expensive to reconstruct but safe to drop:

- rasterized SVG assets at the active scale and theme;
- font glyph atlases and shaped text runs;
- icon and cursor rasters;
- rendered theme primitives;
- window backing stores and recently closed window snapshots, where session
  policy permits them;
- taskbar and compositor asset caches.

Rules:

- the compositor and desktop services use existing shared raster, SVG, theme,
  icon, font, and geometry libraries rather than creating a second rendering
  path;
- headless RustOS retains the same memory-safety and pressure behaviour without
  desktop-specific kernel policy;
- cached window contents are user/session data and must not cross users,
  seats, or sessions;
- UI cache is normally among the first memory released under pressure;
- pressure hints to userland services are added only with current in-tree
  callers, complete capability checks where needed, docs, tests, and validation.

### 6.5 Reliability and recovery assist cache

Reliability cache stores rebuildable helper state that improves recovery or
reduces repeated validation work:

- recent filesystem metadata checkpoints or summaries;
- recent journal or COW verification windows;
- FEC decode assist tables;
- volume health summaries;
- storage-root and alias generation summaries;
- log integrity chain working state;
- dedupe or recompression candidate summaries.

Rules:

- cached recovery state is never the source of truth;
- losing the cache cannot make a valid filesystem invalid or an invalid
  filesystem valid;
- stale generation or authentication failure fails closed;
- any optional lower-tier swap backing remains governed by
  `plans/SWAPSWAPSWAP.md`, `plans/ALIAS.md`, and `plans/DRIVES.md`.

### 6.6 Background validation and maintenance cache

Background cache stores bounded work products from idle-time validation:

- checksum scan progress summaries;
- dedupe candidate fingerprints;
- recompression analysis summaries;
- app signature or content-hash validation results;
- filesystem metadata validation results.

Rules:

- background workers stop immediately on memory pressure, CPU pressure, thermal
  pressure, battery policy, or foreground latency constraints where such signals
  exist;
- workers use event-driven wakeups or one-shot timers, not busy loops;
- background cache cannot grow into reserves or trigger `ramzip` pressure;
- progress summaries are bounded and invalidated by generation.

### 6.7 Predictive prefetch cache

Predictive cache stores data likely to be used soon:

- recently opened app resources;
- recently opened project directory metadata;
- command completion indexes;
- file-browser directory listings;
- thumbnails and previews;
- foreground task resource hints.

Rules:

- prediction is local, bounded, deterministic, and privacy-preserving unless an
  approved design states otherwise;
- prediction never opens data the caller lacks authority to open;
- prediction never blocks foreground tasks;
- predictive cache is disposable under pressure;
- no telemetry service or public tuning surface is added by this plan.

---

## 7. Pressure and reclaim policy

The pressure model must align with the repository's existing VM model. If the
repository already has pressure bands, `smartram` uses them. If it does not, the
stage that introduces pressure states must do so as a complete VM change with
current callers, tests, docs, and validation.

The required shape is:

```text
normal:
  bounded cache growth is allowed while reserves remain protected

mild pressure:
  stop speculative cache growth
  drop disposable UI, predictive, and background-validation cache
  shrink semantic cache entries that are cheap to rebuild
  begin reclaiming clean file cache, matching SWAPSWAPSWAP section 6

moderate pressure:
  finish reclaiming clean file cache and reclaim transform cache
  preserve only hot metadata and recovery-assist entries justified by policy
  then hand pressure to ramzip according to SWAPSWAPSWAP

severe pressure:
  all smartram classes obey forced shrink requests
  ramzip owns cold-anonymous compression policy
  no background validation, predictive prefetch, or UI speculative caching runs

critical pressure:
  no speculative work runs
  forced reclaim completes without panics or retry loops
  escalation follows the VM and SWAPSWAPSWAP pressure policy
```

The implementation must use hysteresis. It must not grow a cache and shrink it
on the same threshold. Exact numbers are not ABI and must be benchmarked, but
initial policy should respect the SWAPSWAPSWAP requirement that clean cache is
cheaper to reclaim than encrypted compressed anonymous storage.

Required invariant:

```text
smartram expansion must never be the cause of reserve exhaustion or ramzip
pressure escalation
```

---

## 8. Accounting and ownership

Every `smartram` entry must be charged to an owner that can be reasoned about.
The first implementation may choose simple owners if they match the existing
kernel model, but it must not create unowned memory.

Required accounting dimensions:

- current bytes by class;
- metadata bytes by class;
- logical bytes represented where different from stored bytes;
- owner contribution;
- per-task or per-address-space contribution where user data is involved;
- per-volume or per-filesystem contribution where storage data is involved;
- current min/soft/hard budgets by class if budgets exist;
- reclaim attempts, successes, refusals, and bytes released;
- invalidations by generation, content change, policy epoch, key epoch, or
  media replacement;
- failures that may indicate corruption, stale identity, authentication
  failure, or permission mismatch;
- interactions with VM pressure state and `ramzip` handoff.

Fairness requirements:

- one user, task, filesystem, app bundle, desktop session, or volume cannot
  push unlimited rebuildable data into global RAM;
- background validation cannot starve foreground tasks;
- desktop cache cannot harm headless builds or kernel memory pressure;
- reliability-assist cache has a budget and is still reclaimable unless a
  concrete safety invariant requires temporary preservation.

---

## 9. Security and privacy

`smartram` improves performance by retaining derived state. It must not create a
new authority path, plaintext graveyard, stale trust decision, or cross-user
leak.

Rules:

- cache lookup never substitutes for capability, ACL, owner/mode, MAC, mount,
  signature, or manifest checks;
- cached authorization-sensitive results include the authority, policy epoch,
  caller scope, and object generation needed to prove they still apply;
- cached decrypted or decompressed user data is protected as user data and
  zeroed when its cache entry is reclaimed, invalidated, or its owner is torn
  down;
- credentials, cryptographic keys, kernel capability metadata, kernel stacks,
  page tables, DMA buffers, MMIO mappings, interrupt stacks, and sensitive
  allocator objects are not cached as `smartram` entries;
- malformed metadata, stale generation, media replacement, authentication
  failure, or policy mismatch fails closed;
- diagnostics and logs expose counters and stable event data, never plaintext
  file contents, page contents, keys, credentials, capability tokens, or private
  per-user filenames unless an existing authorized diagnostic path already
  permits that detail;
- any public diagnostic expansion uses existing capability-checked System
  Information paths only when a current caller requires it.

Every rule in this section is verified by tests that land in the same stage as
the feature it guards: negative tests proving refused access stays refused,
cross-principal and cross-session reuse tests, zeroization checks for reclaimed
plaintext, stale-generation and tampered-state fail-closed tests, and fuzz
targets for any cache-metadata decoder. Passing review is not a substitute for
these tests.

---

## 10. Concurrency and invalidation

Every cache class needs a precise invalidation source. A stale cache entry is a
correctness and security defect, not a performance quirk.

Required invalidation triggers include, where applicable:

- file write, truncate, rename, delete, reflink, COW epoch change, or metadata
  update;
- mount, unmount, remount, key epoch change, policy epoch change, or
  filesystem-driver reload;
- removable media generation change;
- alias, storage-root, or durable-identity generation change;
- user, group, ACL, MAC, capability-authority, or manifest policy change;
- theme, scale, font, icon, or asset source change;
- app bundle content hash, signature, interface hash, or dynamic-loader policy
  change;
- task exit, address-space teardown, session logout, seat revocation, or
  foreground/background class change;
- memory pressure transition to a state that forbids the cache class.

Concurrency requirements:

- reclaim racing lookup is safe;
- invalidation racing rebuild is safe;
- owner teardown racing reclaim is safe;
- media replacement cannot reuse a stale cache entry for the new media;
- authorization-sensitive entries cannot be reused across principals;
- forced reclaim must complete without unbounded waits or retry loops.

---

## 11. Diagnostics and ABI policy

The first implementation should avoid public ABI. Internal counters may be
logged through existing `lib/log` event paths and exposed only through existing,
capability-checked diagnostics if those already support memory statistics and a
current caller needs the information.

If a public query is required, it belongs in the existing System Information API
or an approved successor, not in `/proc`, `/sys`, a text scrape file, or an
ad-hoc syscall. Adding it requires:

- `lib/abi` type updates;
- generated ABI drift checks;
- capability policy;
- rustdoc;
- mdBook docs;
- fuzz tests for decoders;
- `PLAN.md` update;
- full workspace validation.

No user setting may weaken cache invalidation, authorization checks, encryption,
authentication, fail-closed behaviour, reserve preservation, or security
logging.

---

## 12. Staged implementation schedule

Each stage below must land as a complete, tested, documented, fully-gated
change. Do not merge stubs, partial plumbing, ignored tests, speculative public
interfaces, or dead code. Do not add public interfaces before a current in-tree
caller consumes them.

### SMART0 - Planning and documentation only

Deliverables:

- Place this document under `plans/SMARTRAM.md`, or another approved location
  after the repository layout is updated.
- Add a concise reference from `PLAN.md` only if this work is accepted into the
  staged roadmap.
- Add a short cross-reference from `plans/SWAPSWAPSWAP.md` only if reviewers
  want the relationship discoverable from the compressed-memory plan.
- Do not add code in SMART0.

Tests:

- Documentation build and spec-review checks required by the repository.

Docs:

- The plan itself is the deliverable.

### SMART1 - Reclaimable memory classification and accounting model

Deliverables:

- Internal `kernel/mem` model for reclaimable memory classes outside `ramzip`.
- Owner accounting for kernel, filesystem, task/address-space, session, and
  service-owned caches where those owners already exist.
- Bounded metadata model for cache entries.
- Rebuild-cost, sensitivity, invalidation-source, and reclaim-rule modelling.
- Typed refusal reasons for objects that are unknown, unowned, sensitive,
  unbounded, non-reclaimable, or missing invalidation data.
- No userland pressure interface beyond the capability-gated System
  Information export `plans/STRESSTEST.md` ST1 added.

Tests:

- Every cache class maps to a reclaim priority.
- Unknown class rejects.
- Unknown owner rejects.
- Credential/key/capability-sensitive material rejects.
- Unbounded metadata rejects.
- Missing invalidation source rejects.
- Owner accounting cannot underflow or overflow.
- Reclaim decisions are deterministic under equal inputs.
- Production paths contain no panics, unwraps, expects, todos, ignored tests, or
  retry loops.

Docs:

- Update memory architecture docs with the reclaimable memory taxonomy.

### SMART2 - Pressure integration and reclaim ordering

Status: **done.** No VM pressure model existed, so this stage shipped the
complete one (`kernel/mem::pressure`; see
`docs/src/architecture/memory.md` section 7h):

- The five-band `MemoryPressure` gauge (the section 3/7 vocabulary) over a
  `FreeMemorySource` — in production the physical `FrameAllocator`, sampled
  on the consumers' own operations (no background workers, no tick) — with
  per-band enter/exit watermarks derived from the backing size
  (hysteresis; the initial fractions follow the `SWAPSWAPSWAP.md` section 6
  shape and are benchmark-tunable constants, never ABI).
- Reserve preservation: a reserve floor (1/64 of the backing) below which
  every reading is critical, and `growth_permitted`, which admits cache
  growth only at normal pressure and never into the reserve. A zero/unknown
  backing reports critical and admits nothing (fail closed).
- The pure, deterministic reclaim-ordering policy `shrink_target` for every
  class (disposable/speculative drop at mild; clean file begins reclaim at
  mild and drains with transform cache at moderate; metadata and recovery
  assist are preserved to the low watermark; severe/critical force zero),
  plus the `ramzip_handoff` gate (compression only from moderate, and at
  moderate only once clean+transform are drained) and the deterministic
  `escalation` order — the seams `plans/SWAPSWAPSWAP.md` SWAP3 binds to.
- `CachedFs` enforcement: the gauge is threaded from the boot path into
  every mounted volume's cache; every cache-touching operation applies the
  band's shrink targets before serving (data before metadata, evicted
  buffers zeroed) and admission is refused outside normal pressure — the
  driver always keeps serving.

The SMART2 test matrix lands with the stage: pressure-module unit tests
(watermark ordering, hysteresis, growth per band, reserve/zero-backing
fail-closed, determinism, class ordering, handoff, escalation, the
allocator source) and `CachedFs` tests (admission refusal and recovery,
moderate drain preserving metadata, severe full shrink, forced reclaim
racing lookup, owner teardown after forced reclaim).

### SMART3 - Filesystem metadata and transformation caches

Status: **done** for every part with a current in-tree consumer
(`docs/src/architecture/memory.md` section 7i):

- The filesystem **metadata** cache (directory entries, stat, ACL/
  security records, name resolution) is the SMART1 `CachedFs` and is
  live on every registered volume. Extended-attribute and
  type-detection caching are deliberately **not** built: the mounted
  kernel filesystem surface (`KernelFs`) carries no attribute or
  type-detection consumer today, and this stage's deliverables are
  scoped to "where existing filesystem layers have current consumers".
  When an attribute consumer lands, its caching follows
  `plans/ARXFS-METADATA.md` (namespace-scoped capability filtering,
  `lib/fsmeta` as the one key grammar).
- The **ARXFS transform cache** retains the verified, decrypted,
  decompressed plaintext of compressed clusters. The driver exposes an
  injected seam (`rustos_drv_fs_arxfs::ClusterCache`, keyed by the
  run's first stored block) consulted only in the serving read path —
  never by scrub/check/rescue — with invalidation funnelled through the
  driver's single block-free choke point, a whole-cache purge on
  transaction rollback, and a fail-closed `DeviceFault` if an entry
  cannot make progress. The kernel's production implementation
  (`rustos_kernel::transform_cache::TransformClusterCache`) is
  classified through the SMART1 gate (class `TransformCache`, owned by
  the volume's stable per-boot mount handle), LRU-bounded with
  hysteresis, pressure-enforced per operation (preserved at mild,
  drained from moderate, growth only at normal outside the reserve),
  and volatilely wipes every released buffer; the driver additionally
  wipes its transient frame/plaintext scratch on every cluster read,
  clone, and decompose path. Installed on both boot volumes
  (`system_mount` for `/System`, the unlock path for the writable
  root).
- Within one boot the driver instance is the volume generation (mount
  starts empty; every mutation is seen by the one registered writer),
  matching section 6.1; removable-media generation tokens still arrive
  with the storage subsystem that introduces removable volumes. Keys
  are physical-run identities on the mounted volume, never
  discovery-order device names. Permissions are untouched: the cache
  sits below the driver's API and the secured VFS still checks every
  operation.

The SMART3 test matrix lands with the stage: driver-seam tests
(repeat reads served from retained plaintext proven by corrupting the
device after population, overwrite/truncate invalidation through the
free choke point, rollback purge, reflink-share retention, a
wrong-sized entry failing closed instead of stalling) and kernel tests
(classification/owner, hit/miss/insertion accounting, LRU eviction with
hysteresis, run-covering invalidation, replacement ledger balance,
purge, per-band growth/drain enforcement, zero-backing refusal,
wipe-in-place, and an end-to-end serve/invalidate/pressure-drain run
over a real in-memory ARXFS volume).

### SMART4 - Application launch and runtime semantic caches

Status: **done** for every part with a current in-tree consumer
(`docs/src/architecture/memory.md` section 7j):

- The **semantic launch cache**
  (`kernel/core::launch_cache::LaunchCache`) retains the one shared
  `lib/appload` gate's accepted `LoadedApp` — the parsed signed
  `AppInfo` manifest, the content-hash and syscall-interface-hash
  verdicts, the dynamic-loader library policy decisions, and the
  validated `rxe` entry-point image — for bundles on the immutable
  read-only system stores (`/System/Apps`, `/System/Services`), once
  per boot. It is classified through the SMART1 gate (class
  `SemanticAppCache`, owner `KernelSubsystem("app_store")`, expensive
  to rebuild, system data, generation-invalidated, droppable; a refusal
  poisons it from birth), LRU-bounded with hysteresis under the same
  kernel-heap-derived `CacheBudget` as the other volume caches, and
  pressure-enforced per operation (shrunk to the low watermark at mild,
  drained from moderate before any `ramzip` handoff, growth only at
  normal outside the reserve). The `AppStore` holds it behind the
  `/System`-mount readiness latch; `install_system_mount` installs the
  budget and gauge just before resolving the latch, and an uninstalled
  or poisoned cache serves every launch uncached through the full gate
  (fail closed).
- **Scoping and invalidation.** Only immutable read-only store bundles
  are cacheable (`AppStore::cacheable_bundle`); a writable-volume
  bundle is re-verified every launch, so bundle-content, signature,
  interface-hash, and manifest changes can never be served stale.
  Within one boot the read-only store is the generation (an app or
  system update is a new volume image and a new boot). A hit is
  caller-independent by construction: the cached ceiling is the
  manifest request (verified under the full-word intersection
  identity), the per-caller capability intersection happens on every
  admit, and the spawn path re-authorises the caller's read of the
  entry point through the secured VFS before serving a hit — hit and
  miss produce identical load decisions, and no per-principal scoping
  is needed because no caller-dependent data is stored.
- **Scope decisions — deliberately not built.** Command-resolution
  *output* caching is not built: `lib/cmdres` is a pure spelling
  function (no I/O, no permission checks), so recomputing its candidate
  list is cheaper than any cache and would only add staleness risk; the
  expensive work behind a resolved command — the winning candidate's
  verification — is exactly what the launch cache retains, and the
  per-candidate filesystem probes are already served by the SMART1
  `CachedFs` metadata cache. A separate RXE validation/relocation-
  preparation cache is not built: the loader model has no separate
  relocation stage, so the validated image inside the cached
  `LoadedApp` *is* the RXE validation state. Either becomes a new
  consumer-gated deliverable only if a future loader or resolver stage
  introduces the missing expensive step.

The SMART4 test matrix lands with the stage
(`kernel/core/src/launch_cache_tests.rs` plus the spawn-path tests in
`kernel/core/src/syscalls.rs`): classification/owner, hit/miss/
insertion/invalidation/refusal accounting, LRU eviction with
hysteresis, replacement without shadowing, admission refusal outside
normal pressure with recovery, reserve and zero-backing fail-closed
refusal, mild-band shrink to the low watermark, moderate/severe/
critical drain, hit-equals-miss load decisions, reclaim never making an
app unlaunchable, over-long key and over-budget entry refusal, the
uncached-until-install store behaviour, a cached second spawn
performing zero data reads, and a cache hit still authorising the
caller's read (a VFS refusal blocks the cached launch).

### SMART5 - Desktop and UI cache integration

**Status: shelved — not added.** This stage is deliberately not being
implemented. Nothing below is built, tested, or documented unless this
stage is explicitly un-shelved by a future decision.

Deliverables:

- Desktop/session-owned caches for SVG rasterization, theme primitives, font
  glyphs, icon/cursor rasters, shaped text runs, compositor surfaces, and
  optional window snapshots where session policy permits them.
- Pressure-aware cache trimming for WM, taskbar, and desktop session using an
  existing signal path or a complete in-tree pressure interface with current
  callers.
- Shared rendering path through existing raster, SVG, icon, font, theme, and
  geometry crates.
- Headless configuration remains fully supported and does not depend on desktop
  services.

Tests:

- Scale or theme change invalidates rasterized assets.
- Font or icon source change invalidates dependent entries.
- Session logout or seat revocation drops session-owned UI cache.
- Cached window contents never cross users, seats, or sessions.
- Headless build does not instantiate desktop cache policy.
- UI cache is released under pressure before `ramzip` escalation.
- The compositor does not add a second raster or blend implementation.

Docs:

- Update desktop and memory docs with UI cache policy, privacy boundaries, and
  headless behaviour.

### SMART6 - Reliability and recovery assist caches

**Status: shelved — not added.** This stage is deliberately not being
implemented. Nothing below is built, tested, or documented unless this
stage is explicitly un-shelved by a future decision.

Deliverables:

- Bounded reliability-assist cache for filesystem health summaries, recent
  verification windows, COW/journal summaries, FEC decode assist state, and
  volume generation summaries where the relevant filesystem/storage code
  exists.
- Clear distinction between cached helper state and canonical on-disk or
  in-memory truth.
- Fail-closed handling for stale generation, invalid authentication, invalid
  checksum, and media replacement.
- No dependency on optional lower-tier swap internals.

Tests:

- Dropping reliability cache cannot corrupt valid data.
- Stale recovery summary fails closed.
- FEC helper state invalidates on generation change.
- Filesystem health summary cannot mark an invalid object valid.
- Forced reclaim preserves correctness.
- Media replacement cannot reuse recovery state from the removed medium.

Docs:

- Update storage, filesystem, and memory docs with recovery-assist semantics.

### SMART7 - Background validation and maintenance caches

**Status: shelved — not added.** This stage is deliberately not being
implemented. Nothing below is built, tested, or documented unless this
stage is explicitly un-shelved by a future decision.

Deliverables:

- Bounded background workers for checksum validation, dedupe candidate
  discovery, recompression analysis, app signature validation, and filesystem
  metadata validation where existing subsystems have current consumers.
- Event-driven or one-shot-timer scheduling; no busy loops.
- Immediate stop on memory pressure, CPU pressure, thermal pressure, battery
  policy, or foreground latency constraints where such signals exist.
- Bounded progress summaries and invalidation by generation.

Tests:

- Background workers do not run under pressure.
- Workers stop immediately when pressure appears.
- Workers respect CPU, byte, page, and time budgets.
- Workers preserve reserves and do not trigger `ramzip` pressure.
- Progress summaries invalidate on generation change.
- No busy-poll loop is introduced.

Docs:

- Update memory and filesystem docs with background validation budgets and stop
  conditions.

### SMART8 - Predictive workflow and prefetch caches

**Status: shelved — not added.** This stage is deliberately not being
implemented. Nothing below is built, tested, or documented unless this
stage is explicitly un-shelved by a future decision.

Deliverables:

- Bounded prefetch and prediction cache for recently used app resources,
  project directories, command completion indexes, file-browser listings,
  thumbnails, and previews.
- Per-principal and per-session scoping where results reveal private data.
- Strict authority checks before any data is opened or prefetched.
- Disposable reclaim policy under pressure.
- No telemetry service or public tuning UI.

Tests:

- Prefetch refuses data the caller cannot open.
- Predictive entries are scoped to the correct user/session.
- Session logout drops private predictive state.
- Pressure drops predictive cache before clean filesystem cache and before
  `ramzip` handoff.
- Prediction cannot block foreground work.
- Thumbnail or preview cache invalidates on source generation change.

Docs:

- Update shell, desktop, and memory docs with predictive-cache scope and
  privacy rules.

### SMART9 - Observability through existing diagnostics

**Status: done.** The subsystem is observable through internal counters
and existing structured logging; `plans/STRESSTEST.md` ST1 has since
exported the same figures through the capability-gated
`MEMORY_PRESSURE`/`RECLAIM_STATS`/`RAMZIP_STATS` System Information
queries (the in-tree callers SMART9 anticipated). No
`/proc`/`/sys`/text-scrape path exists.

What now holds (`docs/src/architecture/memory.md` §7k):

- `kernel/mem::reclaim::CacheAccounting` splits each class's byte
  ledger into payload and per-entry bookkeeping metadata
  (`class_payload_bytes` / `class_metadata_bytes`; budgets bound the
  sum) and counts, beside hits/misses/insertions/invalidations/
  evictions/refusals: `pressure_shrinks` (forced-shrink passes that
  reclaimed), `teardowns` (whole-cache drains), and `failures`
  (detected ledger/index defects). Each cache instance is charged to
  exactly one `ReclaimOwner`, so its ledger is that owner's
  contribution.
- `kernel/mem::pressure::MemoryPressure` counts entries into each band
  (`band_entries`, one atomic per band, swap-exact per stored change;
  the starting band and hysteresis holds count nothing).
- `kernel/mem::reclaim_audit` owns the subsystem's stable audit events
  in the reserved `2_000..3_000` range: `RECLAIM_CACHE_REFUSED` (2000)
  for a classification-gate refusal at construction and
  `RECLAIM_CACHE_POISONED` (2001) for a live cache's detected
  ledger/index defect. The field shape is closed — `cache` label,
  `owner` kind/name, numeric `owner_id`, `cause` label — so no
  filename, plaintext, key, or capability token can enter a record.
  All three caches (`CachedFs`, `TransformClusterCache`,
  `LaunchCache`) take the boot audit sink at construction and report a
  poisoning exactly once; normal operation emits nothing.
- Tests land beside each piece: counter-per-path and ledger-split
  coverage in `kernel/mem` and all three cache test suites, transition
  counting in the pressure tests, one-shot poison reporting with the
  closed field shape, and a no-records-in-normal-operation check per
  cache.

### SMART10 - Integration, benchmarks, and full validation

**Status: done** for the implemented stages (SMART1–SMART4 + SMART9;
the shelved SMART5–SMART8 rows of the section 13 matrix apply only if
those stages are ever un-shelved). See
`docs/src/architecture/memory.md` §7l for the binding description.

What now holds:

- **Cross-cache integration over one shared gauge**
  (`kernel/core/src/reclaim_integration_tests.rs`): the production
  `CachedFs` and `LaunchCache` driven through the full band order from
  a single simulated free-memory source; the `ramzip` handoff computed
  over the caches' combined clean+transform residue (held while any
  remains, open once their own operations drain it, never at critical
  — escalation yields the VM policy); the shared reserve floor; and
  no-stale-serving for a file mutated while the caches were drained.
- **The layered stack** (`kernel/rustos-kernel/src/`
  `transform_cache_tests.rs`): `CachedFs` over a real ARXFS volume
  whose read path consults the installed `TransformClusterCache`, both
  on one gauge — a filesystem-cache hit never reaches the transform
  layer, and moderate pressure drains both layers while correct bytes
  keep being served through the full transform pipeline.
- **Thrash scenario**: band flapping inside the mild hysteresis window
  causes zero rebuild churn (flat insertion counters, counted
  refusals, no repeated band entries); one genuine recovery rebuilds
  once. Churn is detected through the SMART9 counters and reduced by
  hysteresis plus outside-normal admission refusal — no new mechanism.
- **Benchmark evidence** (section 14): deterministic work-avoided
  assertions (a warm pass performs zero driver reads and zero
  load-gate runs) plus printed wall-clock estimates for warm and cold
  passes — estimates for threshold tuning, never guarantees. The
  band watermarks and budget fractions stay implementation constants.
- **Shared fixtures live once**: the controllable gauge source
  (`kernel/core/src/test_pressure.rs`) and the bundle-verification
  helpers (`kernel/core/src/test_bundle.rs`) are the single
  definitions every cache suite imports.

Scope decisions recorded:

- A dedicated QEMU pressure-band vertical is not built: the gauge's
  band arithmetic is pure and host-proven, and the frame allocator it
  samples is already exercised by the existing
  `tests/integration/memsoak_qemu_aarch64` soak.
- Removable-media generation and multi-seat/desktop rows of the
  section 13 matrix stay with their owning staged/shelved stages
  (removable media has no kernel subsystem yet; SMART5–SMART8 are
  shelved); the multi-user authorization rows are held by the
  per-cache suites (authorisation-sensitive reuse in `CachedFs`,
  hit/miss-identical decisions in `LaunchCache`).

### SMART11 - Whole-disk block-level LRU cache

Status: **done** (`docs/src/architecture/memory.md` section 7m). The
filesystem block-level cache the section 6.1 model implies below the
volume layer, subject to the same pressure-integration reclaim
ordering as every other class:

- `kernel/rustos-kernel::block_cache::BlockCache` wraps the one
  brought-up boot disk **below** the block-sharing layer
  (`shared_block::SharedBlock`), on the device side of its sleep
  lock, so every window onto the disk — the `/System` driver-store
  window, the encrypted-root unlock window, and the writable-root
  window — reads through one coherent per-block LRU cache and every
  mutation any window issues is observed serialised. Installed by the
  shared `finish_unlock` boot tail (both the virtio-blk and EMMC2
  bring-ups), threaded the same gauge and audit sink as the volume
  caches.
- Classified through the SMART1 gate as `CleanFileData` (cheap to
  rebuild — one bounded device read), owned by the
  `boot_block_device` kernel subsystem, treated as user data (the
  disk carries the encrypted user volume), source-mutation
  invalidated, droppable; a refusal — or a device block size the
  per-block entry model cannot bound — poisons the cache from birth
  and every operation passes straight through (fail closed).
- Pressure-enforced per operation under the SMART2 gauge: shrunk to
  the low watermark at mild pressure, drained to zero from moderate
  on (before any `ramzip` handoff, matching section 7), growth only
  at normal pressure outside the reserve, LRU eviction to the low
  watermark on a full budget (hysteresis).
- Coherence: a successful write refreshes cached copies in place
  (admitting nothing new), a failed write invalidates its range (the
  device state is unknown), a discard invalidates its range, and
  reads wider than the large-read bound stream through uncached so a
  bulk bundle/driver-store load cannot flush the hot working set.
- Secret hygiene: `BufferClass::Sensitive` reads and writes bypass
  the cache entirely *and* evict any cached copy of their range, so
  no key-slot or credential-bearing block is ever retained; every
  released buffer is volatilely wiped.
- Observability rides SMART9 unchanged: cache label `block`, the
  split payload/metadata ledger, and the one-shot 2000/2001 audit
  events.

The SMART11 test matrix lands with the stage
(`kernel/rustos-kernel/src/block_cache_tests.rs`): classification/
owner, hit/miss/insertion accounting with a device-corruption proof
that a hit never reaches the device, multi-block partial-hit
behaviour, write-through coherence, failed-write and discard
invalidation, sensitive-class scrubbing on both directions,
non-sensitive classified reads cached normally, large-read bypass,
unaligned passthrough, LRU eviction with hysteresis, per-band
growth/shrink/drain enforcement with recovery, zero-backing refusal,
uncacheable-geometry poisoning with the device still serving,
geometry-fault refusal to wrap, forwarding of geometry/discard
capability/health, split ledger accounting, one-shot closed-shape
audit reporting, silence in normal operation, and wipe-in-place.

Scope decisions recorded:

- The cache is installed on the one persistent shared boot disk (the
  only whole-disk device the kernel keeps mounted today); additional
  disks receive the same wrap when the storage subsystem that
  introduces them lands.
- Removable-media generation tokens stay with the storage subsystem
  that introduces removable volumes (section 6.1): within one boot
  the wrapped device instance is the generation, and the cache dies
  with it.

---

## 13. Required test matrix summary

The completed feature must include tests for the implemented and staged
stages. The `UI and desktop cache`, `reliability and background cache`,
and `predictive cache` blocks below belong to the shelved SMART5–SMART8
stages and apply only if those stages are ever un-shelved:

```text
classification:
  known cache classes accepted
  unknown cache class rejected
  unknown owner rejected
  missing invalidation source rejected
  unbounded metadata rejected
  credential/key/capability-sensitive entries rejected

accounting:
  bytes accounted by class
  metadata bytes accounted by class
  logical bytes accounted where different from stored bytes
  owner contribution accounted
  per-task/per-address-space contribution accounted for user data
  per-volume/per-filesystem contribution accounted for storage data
  no underflow or overflow on reclaim
  owner teardown drains or transfers ownership safely

pressure:
  normal state permits bounded growth
  mild pressure stops speculative growth
  disposable cache drops first
  clean file and transform cache drop before ramzip compression
  severe pressure forces shrink
  critical pressure starts no speculative work
  reserves are preserved
  ramzip handoff follows SWAPSWAPSWAP

filesystem and transform cache:
  directory/stat/ACL metadata invalidates on change
  durable identity keys do not use discovery-order device names
  removable-media generation change invalidates entries
  verified-state tamper fails closed
  decrypted user data is never exposed through diagnostics
  forced reclaim preserves filesystem correctness

semantic app/runtime cache:
  app content change invalidates launch state
  signature change invalidates validation state
  interface-hash change invalidates validation state
  policy/capability change prevents stale reuse
  cache hit and miss make the same authorization decision
  malformed cached state fails closed

UI and desktop cache:
  theme change invalidates theme-derived assets
  scale change invalidates rasterized assets
  session logout drops private UI cache
  seat revocation drops seat-owned cache
  headless build remains valid
  no second raster/blend path is introduced

reliability and background cache:
  cached recovery state is not source of truth
  stale recovery state fails closed
  background worker stops under pressure
  background worker respects CPU/byte/time budgets
  background worker does not trigger ramzip pressure
  no busy loop is introduced

predictive cache:
  authority check precedes prefetch
  private results are scoped by user/session
  source generation change invalidates previews and thumbnails
  pressure drops predictive cache deterministically
  foreground tasks are not blocked

observability:
  counters update consistently
  security-relevant failures log stable events
  diagnostics expose no plaintext or secrets
  public ABI absent unless approved and fully tested

concurrency:
  lookup racing reclaim is safe
  rebuild racing invalidation is safe
  owner teardown racing reclaim is safe
  media replacement racing lookup is safe
  pressure transition racing background worker is safe
  repeated rebuild/reclaim churn is detected and reduced
```

---

## 14. Benchmarks and performance evidence

Every implemented cache class and policy is tested with benchmarks.
Implementation is incomplete without performance evidence for any policy choice
that affects thresholds, budgets, reclaim order, cache retention, or foreground
latency.

Required benchmark areas (the desktop asset render, background
validation, and predictive prefetch entries belong to the shelved
SMART5–SMART8 stages and apply only if those stages are ever
un-shelved):

- cache lookup latency by class;
- cache insert and invalidation cost by class;
- reclaim latency per class;
- memory saved or repeated work avoided per workload class;
- filesystem metadata-heavy workload;
- ARXFS compressed/encrypted workload where ARXFS supports it;
- application launch cold/hot comparison;
- command-resolution cold/hot comparison;
- desktop asset render cold/hot comparison;
- headless build overhead;
- background validation CPU and memory cost;
- predictive prefetch benefit and wasted-work rate;
- interaction with `ramzip` under moderate and severe pressure;
- small-RAM profile;
- desktop/laptop larger-RAM profile;
- wasm target behaviour where the cache model is represented there;
- worst-case churn where cached objects are repeatedly invalidated.

Benchmarks report estimates as estimates, not guarantees. Any default budget,
threshold, or reclaim priority chosen because of benchmark data must cite the
benchmark in docs or completion notes.

---

## 15. Acceptance checklist

This work is complete only when all applicable items are true:

- `AGENTS.md` has been read and remains the superior contract.
- `plans/SWAPSWAPSWAP.md` has been read, and `ramzip` behaviour is referenced
  rather than duplicated.
- `plans/ALIAS.md` and `plans/DRIVES.md` have been read, and storage identity
  keys use durable identity and generation semantics where storage is involved.
- `plans/ARXFS-METADATA.md` has been read, and any extended-attribute caching
  preserves its namespace capability filtering and reuses `lib/fsmeta` rather
  than duplicating the key grammar or preset registry.
- `PLAN.md` has been updated if a staged roadmap item advanced.
- README has been reviewed if an implemented feature affects a support matrix
  or feature description.
- The implementation is Rust-only.
- No C, C++, hand-written headers, build glue in another language, or new
  assembly was added.
- No public ABI was added unless a current caller required it and the ABI work
  was fully documented, tested, generated, fuzzed, and drift-checked.
- No new parser, renderer, path resolver, app loader, command resolver,
  compression layer, crypto path, or diagnostics path duplicates existing
  shared crates.
- Unknown, unowned, sensitive, unbounded, or non-reclaimable entries fail
  closed.
- Cached authorization-sensitive results cannot bypass capability, ACL, MAC,
  mount, manifest, or signature checks.
- Cached decrypted or decompressed user data is protected as user data and
  zeroed when reclaimed or invalidated.
- Diagnostics expose no plaintext, keys, credentials, capability tokens, or
  unauthorized private filenames.
- Reclaim preserves emergency and decompression reserves.
- Clean and rebuildable cache is reclaimed before cold anonymous pages are
  compressed into `ramzip`, unless the repository's VM policy documents a
  stricter ordering.
- Background work is event-driven or timer-driven, budgeted, and unable to
  recreate pressure.
- Predictive cache is authority-checked, scoped, disposable, and
  privacy-preserving.
- Headless RustOS remains first-class.
- No production `unwrap()`, `expect()`, `panic!()`, `todo!()`, ignored test,
  sleep loop, retry loop, global mutable static, or commented-out test was
  introduced.
- Any `unsafe` is justified, encapsulated, and tested.
- Unit tests, integration tests, fuzz/property tests, docs, and benchmarks land
  with the code.
- Every security feature is checked with dedicated tests (fail-closed refusal,
  capability filtering, cross-principal isolation, zeroization, stale-identity
  rejection), and every performance-based policy choice is backed by benchmark
  evidence.
- The whole-project gate has been run in the foreground to completion:
  `cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
  `cargo xtask fuzz --secs 5`, and any other `.github/workflows/ci.yml` command
  not covered by those commands, including the developer-machine soak cap where
  applicable.
- The completion report quotes the actual command output and states the
  `AGENTS.md` verdict.

---

## 16. Prompt for an implementation agent

Use this prompt when RustOS is ready to implement the next approved stage:

```text
You are implementing the next approved stage of `plans/SMARTRAM.md` for
RustOS.

Before coding, read `AGENTS.md`, `PLAN.md`, `plans/SMARTRAM.md`,
`plans/SWAPSWAPSWAP.md`, `plans/ALIAS.md`, `plans/DRIVES.md`,
`plans/ARXFS-METADATA.md`,
`docs/src/architecture/memory.md`, `docs/src/architecture/security.md`,
`docs/src/filesystem/drives.md`, the filesystem docs relevant to ARXFS,
`kernel/mem`, `kernel/core`, `kernel/sched`, `kernel/sec`, existing VM pressure,
page-reclaim, OOM, page-cache, filesystem-cache, logging, and diagnostics code,
and every shared library that already owns app loading, command resolution,
path parsing, rendering, compression, crypto, logging, theme, icon, font,
raster, or metadata behaviour for the stage you are touching.

State the assumptions you verified from the repository: current pressure model,
reclaim order, reserve model, page-cache ownership, filesystem invalidation
model, storage identity and generation model, scheduler pressure hooks,
userland pressure-signal path if one exists, app-load cache seams, desktop cache
seams, diagnostics surface, and whether any public ABI is already required by a
current in-tree caller.

Implement only the approved stage. Do not add stubs, `todo!()`, ignored tests,
`#[allow(...)]` silencing, speculative public ABI, public syscalls, public
capabilities, disk swap, hibernation, `/proc`, `/sys`, C code, C++ code,
hand-written headers, build glue in another language, or hand-written assembly.
Do not add a new dependency unless the dependency policy, roadmap, workspace,
docs, license/advisory audit, and tests are updated in the same change.

The design target is a bounded, reclaimable, owner-accounted memory-services
pool for clean filesystem data, metadata, transform cache, semantic app/runtime
cache, UI cache, reliability assist cache, background validation, and predictive
prefetch. The encrypted compressed anonymous-memory tier remains governed by
`plans/SWAPSWAPSWAP.md`; reclaim clean/rebuildable cache before handing pressure
to `ramzip`, preserve reserves, stop speculative work under pressure, fail
closed on stale identity or corrupted state, and never use cached data to bypass
permissions, capabilities, signatures, encryption, authentication, or policy.

Finish by running the full workspace gate in the foreground and waiting for it
to exit: `cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
`cargo xtask fuzz --secs 5`, and anything else `.github/workflows/ci.yml` runs
that those commands do not already cover. On a developer machine, any soak run
must use the AGENTS.md developer cap. Quote actual command output and state the
AGENTS.md verdict.
```
