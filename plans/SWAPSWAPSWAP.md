# SWAPSWAPSWAP.md - Encrypted compressed memory pressure tier

Status: SWAP1–SWAP4 implemented — the `ramzip` tier is complete as the
architecture-neutral VM mechanism (`kernel/mem::ramzip`: fail-closed
eligibility, derived min/soft/hard caps, checked per-task accounting,
compress-before-encrypt authenticated store over the shared
`kernel/mem::seal` primitives, pressure-gated compress-out with
decompression-floor preservation, move-only fault-in, bounded
clustering and warm-up, deterministic thrash detection and escalation;
see `PLAN.md` §SWAPSWAPSWAP and `docs/src/architecture/memory.md` §7n
for the done-state summaries). Enabling it for arbitrary *running*
tasks awaits the restartable-user-page-fault prerequisite staged in
`PLAN.md` (every port's fault hook is terminal today), and SWAP5 (the
optional lower-tier block swap) remains a separately approved future
design (section 15)  
Target: RustOS  
Primary code area: `kernel/mem`  
Secondary code areas: `kernel/sched`, `kernel/sec`, `kernel/core`, `lib/crypto`, `lib/log`, and existing `lib/abi` diagnostics only if a current caller requires them  
Repository placement: `plans/SWAPSWAPSWAP.md`, unless `AGENTS.md` section 3 is updated to permit another location

This document captures the RustOS design direction for a near-zero-idle-cost,
encrypted, compressed anonymous-memory tier. It is written so an implementing
agent can use it later without re-litigating the design intent from the
conversation.

This is not a changelog and not a merge-ready implementation. It is a design,
invariant, staging, and acceptance document. If this document conflicts with
`AGENTS.md`, `AGENTS.md` wins. If this document conflicts with the repository's
actual memory manager, scheduler, security, ABI, or documentation state, the
implementing agent must surface the mismatch before implementing a guess.

---

## 1. Source of truth

Before touching code, the implementing agent must read and reconcile:

1. `AGENTS.md`.
2. `PLAN.md`.
3. `docs/src/architecture/memory.md`.
4. `docs/src/architecture/security.md`.
5. `kernel/mem`.
6. `kernel/sched`.
7. `kernel/sec`.
8. `kernel/core`.
9. `lib/crypto`.
10. `lib/log`.
11. Any existing compression abstraction, if one exists.
12. Any existing VM pressure, page-fault, page-reclaim, OOM, or per-process
    memory-accounting code.
13. `plans/ALIAS.md` (resource alias and selector namespaces). It owns how
    non-filesystem resources and raw storage devices are named (`disk:`,
    `part:`, `vol:`, fingerprints, pinned aliases, generation checks).
14. `plans/DRIVES.md` (storage root, volume, alias, and path namespace model).
    It owns durable storage identity (`id::`), the storage-root forest, and
    the rule that a healthy volume stays openable without the single `/` root
    view. Any optional lower-tier swap backing (see section 15) is named and
    resolved only through these two contracts, never through discovery-order
    or board-specific names. Where they conflict with this document,
    `AGENTS.md` still wins, then `ALIAS.md`/`DRIVES.md` over this file.

RustOS rules that matter especially for this work:

- Every line of RustOS implementation is Rust, except the narrow assembly
  carve-out already allowed by `AGENTS.md`. This feature must not add C, C++,
  hand-written assembly, build glue in another language, or generated headers.
- Security and correctness are the floor; performance is a first-class goal
  inside that floor.
- Do not add public ABI, syscalls, driver traits, capability constants, or
  userland services unless there is a current in-tree caller and the ABI,
  docs, tests, and `PLAN.md` changes land in the same fully-gated change.
- Do not duplicate constants, pressure thresholds, page-state algebra,
  compression metadata formats, crypto nonce logic, accounting code, or
  eligibility checks across architecture backends or sibling implementations.
- Do not use `unwrap()`, `expect()`, `panic!()`, `todo!()`, ignored tests, sleep
  loops, retry-until-it-works loops, global mutable statics, or commented-out
  tests in production paths.
- Any `unsafe` required by low-level memory-management code must have a
  `// SAFETY:` block, be encapsulated behind a safe API, and be covered by a
  unit test or model check.
- Every implementation stage must ship with rustdoc, mdBook documentation,
  unit tests, integration tests where applicable, fuzz/property tests where
  applicable, and the whole-project validation gate required by `AGENTS.md`.
- Any storage backing this feature ever names (only the optional lower-tier
  swap of section 15) is referenced by durable identity through the
  `ALIAS.md`/`DRIVES.md` namespaces, not by discovery-order names
  (`disk:0`, `dev:sda`, `/Storage/<n>` ordering) and not by a board or SoC
  name. Resolution yields a capability, never a pathname, and never depends
  on a single root filesystem view.

---

## 2. Goal

RustOS shall provide an encrypted, compressed, RAM-resident tier for cold
anonymous pages before any optional block-device swap is used.

The feature's purpose is to make memory pressure survivable and responsive
without turning removable or fragile storage, especially Raspberry Pi SD cards,
into the first pressure sink.

The intended tiering model is:

```text
active RAM
  -> encrypted compressed anonymous-memory tier
  -> optional encrypted block/ARXFS swap, only if configured later
  -> pressure policy: reclaim, freeze, kill, or OOM
```

The internal working name in this document is `ramzip`. The production name may
differ, but the semantics must remain clear: this is a compressed memory tier,
not magic extra RAM and not ordinary persistent swap.

---

## 3. Non-goals

This work does not implement:

- A disk-swap subsystem.
- A ARXFS swap file format.
- Hibernation.
- Persistent crash dumps.
- A user-visible tuning UI.
- A Linux-compatible `/proc`, `/sys`, `swapon`, `zramctl`, or `zswap` surface.
- A public ABI unless an approved current caller requires it.
- A claim that encrypted in-RAM pages protect against a fully compromised
  running kernel.

Block swap may be designed later. If it lands, it is lower priority than
`ramzip`, separately encrypted, fail-closed, and forbidden from relying on the
RAM tier's key or metadata format.

---

## 4. Design summary

`ramzip` starts with near-zero idle cost. At boot it establishes accounting,
metadata bounds, and a small emergency capacity guarantee, but it must not
permanently steal a large region of RAM.

The default capacity policy is:

```text
minimum guaranteed capacity = max(1% of physical RAM, 64 MiB)
soft cap                    = platform-profile constant, initially about 10% RAM
hard cap                    = platform-profile constant, initially about 25% RAM
```

The minimum is a guarantee that the VM has somewhere safe to put cold pages
under sudden pressure. It is not permission to eagerly allocate and hide 64 MiB
from a 512 MiB machine.

The physical storage backing compressed blobs is allocated lazily from ordinary
page frames while preserving emergency reserves. The implementation may keep a
small preallocated chunk or metadata pool if tests prove it is required to make
forward progress under sudden pressure, but that pool is part of the same cap
and must be justified with tests.

The page pipeline is:

```text
anonymous page
  -> eligibility check
  -> compress
  -> encrypt and authenticate compressed bytes
  -> store encrypted blob in RAM-resident pool
  -> replace PTE with compressed-entry marker
```

The fault-in pipeline is:

```text
compressed-entry fault
  -> locate encrypted blob
  -> authenticate and decrypt
  -> decompress
  -> restore page
  -> erase temporary plaintext/compressed buffers
```

Compression must happen before encryption. Encrypting before compression is
forbidden because encrypted data should be high entropy and should not compress
usefully.

---

## 5. Page eligibility

Only cold anonymous user pages are first-class candidates.

Eligible pages are:

- anonymous user pages;
- not recently accessed by the page-replacement policy;
- unpinned;
- not DMA-visible;
- not mapped as device memory;
- not marked realtime, latency-critical, or never-compress;
- not part of page tables, kernel stacks, interrupt stacks, or kernel-critical
  metadata;
- not known to contain kernel-owned secrets or capability-critical material.

Ineligible pages include, at minimum:

- kernel stacks;
- interrupt stacks;
- page tables;
- DMA buffers;
- MMIO or device-memory mappings;
- driver ring buffers;
- cryptographic key storage;
- kernel credential, token, or capability metadata;
- pages of a process pinned through `mem_pin` (the `CAP_MEM_PIN`-gated,
  `pinned-memory-bytes`-bounded whole-process pin, `plans/STRESSTEST.md`
  ST2 — the per-task registry's pin mark is the classifier's `pinned`
  attribute source), or marked sensitive;
- foreground realtime audio, input, compositor, or scheduler-critical pages;
- pages whose type is unknown.

Unknown is ineligible. The eligibility check must fail closed.

Clean file cache should generally be dropped or reclaimed before cold anonymous
pages are compressed. Reconstructable clean cache is cheaper than encrypted
compressed anonymous storage.

---

## 6. Pressure policy

`ramzip` must not wait until RustOS is one allocation away from OOM. At that
point the kernel still needs memory for interrupts, page faults, page tables,
logging, scheduling, decompression, storage I/O, and OOM recovery.

The implementation shall use pressure bands with hysteresis. Exact numbers are
implementation constants that must be benchmarked and documented, but the shape
is mandatory:

```text
normal:
  no ramzip activity

mild pressure:
  reclaim clean file cache and disposable caches first

moderate pressure:
  compress cold eligible anonymous pages into ramzip

severe pressure:
  grow ramzip toward its hard cap while preserving reserves

critical pressure:
  stop speculative work; use configured lower-tier swap if approved, freeze or
  kill selected tasks, or OOM cleanly
```

The system must not use the same threshold for compression and decompression.
A possible initial shape is:

```text
start compression:          free RAM below about 8-10%
stop compression:           free RAM above about 12-15%
allow background warm-up:   free RAM above about 20-25%
stop background warm-up:    free RAM below about 15-18%
```

Those numbers are not ABI. They are initial implementation targets that must be
validated with tests and benchmarks. If the repository already has a memory
pressure model, the implementing agent must use that model rather than invent a
parallel vocabulary.

---

## 7. Reserves and deadlock prevention

`ramzip` must never consume:

- the emergency allocator reserve;
- the interrupt-handling reserve;
- the page-fault handling reserve;
- the decompression reserve;
- the scheduler/OOM recovery reserve;
- the logging reserve needed to record security-relevant failures;
- storage-I/O reserves needed by any optional lower-tier swap path.

A compressed page fault needs free memory to restore the page. Therefore the VM
must maintain a hard decompression reserve at all times. If preserving that
reserve means refusing to compress another page, the correct result is a normal
error or pressure-policy escalation, not a panic.

Required invariant:

```text
ramzip expansion must never be the cause of reserve exhaustion
```

All allocation failures must be surfaced as typed `Result` errors through the
existing RustOS error vocabulary. Production paths must not panic.

---

## 8. Compression

The compression policy must prefer predictable latency over maximum ratio.

Acceptable first implementation choices are:

- an existing approved RustOS compression abstraction, if present;
- a new in-tree `no_std` compression crate only if at least two production
  crates need it and `AGENTS.md`, `PLAN.md`, workspace membership, docs, and
  tests are updated in the same change;
- a vetted external codec only if the dependency audit concludes that rolling
  our own would be less safe or correct, and the dependency is pinned,
  license-checked, advisory-audited, wrapped, documented, and approved.

Heavy compression levels are not appropriate for the hot memory-pressure path.
The implementation should target LZ4-class latency or a first-party
zstd-fast-style profile if RustOS already has such a layer.

Compression granularity should be one page or a small cluster. Huge regions are
forbidden in v1 because they make random page faults expensive and complicate
reserve accounting.

If a page is incompressible or expands after compression, v1 should normally
reject it as a `ramzip` candidate rather than storing it raw. Storing raw pages
inside `ramzip` defeats the tier's purpose and increases pressure. A later
implementation may store raw only if tests prove a safety or latency need and
the cap accounting treats it as an expensive entry.

---

## 9. Encryption and authentication

The tier must not create a plaintext graveyard of old process memory.
Compressed blobs are encrypted before being stored in the RAM pool.

The security target is limited and explicit:

- reduce stale plaintext exposure inside allocator reuse paths;
- reduce accidental plaintext exposure in crash-dump, hibernation, or future
  persistence paths;
- reduce damage from some physical or DMA-style attacks;
- make memory corruption or tampering fail closed when authentication is used.

This does not defend against a fully compromised running kernel that can read
keys or plaintext before encryption.

The cryptographic rules are:

- Use `lib/crypto` wrappers only. Do not hand-roll cryptographic primitives.
- Use a per-boot `ramzip` master key generated by the kernel from the approved
  RustOS entropy path.
- Derive or assign a unique nonce per compressed entry.
- Prefer AEAD/authenticated encryption per entry. Encryption without
  authentication requires a benchmark-backed and security-reviewed exception.
- Authentication failure must never return plaintext. It fails closed, logs the
  event, and escalates through the memory-fault/OOM policy.
- Temporary plaintext and compressed plaintext buffers must be zeroed before
  reuse or free.
- Metadata that can affect bounds, ownership, nonce selection, or page identity
  must be validated before use and protected against replay inside the same
  boot where applicable.

The implementation must prove nonce uniqueness with tests or a model. Reusing a
nonce under the same key is a security defect.

---

## 10. Metadata and accounting

`ramzip` metadata must be bounded, typed, and testable.

Required accounting dimensions:

- physical RAM budget consumed by encrypted blobs;
- metadata bytes;
- logical page bytes represented;
- compressed bytes before encryption overhead;
- encrypted stored bytes after authentication overhead;
- per-task or per-address-space contribution;
- current min/soft/hard cap usage;
- compression attempts, acceptances, rejections, and fault-ins;
- authentication failures;
- decompression failures;
- warm-up attempts, acceptances, rejections, and cancellations;
- pressure-state transitions.

Per-process accounting is required so one process cannot push unlimited cold
memory into `ramzip` and externalise the cost to the rest of the system. The
policy may be simple in v1, but it must exist.

The compressed-entry marker in page tables must be an architecture-neutral VM
state, not a target-specific one-off. Architecture-specific PTE encodings must
remain under the architecture backends and expose a common safe interface to
`kernel/mem`.

---

## 11. Fault-in and fault clustering

On a compressed-page fault, RustOS restores the requested page first.

After the requested page is restored, the VM may decompress a very small nearby
cluster if all of the following are true:

- memory is comfortably above the warm-up threshold;
- the decompression reserve remains protected;
- the faulting process is active or foreground-relevant;
- adjacent pages belong to the same mapping or compatible VM region;
- the pages were compressed near the same time or have evidence of locality;
- CPU pressure is low;
- no realtime or interactive latency constraint is active.

Fault clustering should begin conservatively, for example the faulted page plus
1-8 nearby pages, with an absolute byte budget per event. Exact values are not
ABI and must be benchmarked.

Cluster failure must not fail the original fault once the original page has
been restored. Cluster work is opportunistic.

---

## 12. Background decompression

Background decompression is allowed only as a latency optimisation. It is not a
cleanup policy.

RustOS must not decompress the whole tier merely because pressure has gone
away. Keeping cold pages compressed preserves RAM for active working sets and
file cache.

A background warm worker may run only when:

- free RAM is comfortably above the warm-up threshold;
- CPU load is low;
- no memory pressure, CPU pressure, thermal pressure, or battery policy blocks
  it;
- no realtime or foreground latency constraint is active;
- candidates have evidence of near-future use.

Candidate priority:

1. Pages recently faulted from `ramzip` and then re-compressed.
2. Pages near recently faulted pages in the same VM region.
3. Pages belonging to foreground interactive tasks.
4. Pages belonging to tasks recently resumed from background or sleep.
5. Everything else remains compressed.

The worker must have strict budgets:

- maximum pages or bytes per batch;
- maximum pages or bytes per second;
- maximum CPU time per scheduling window;
- immediate stop on any pressure transition;
- immediate stop if the decompression reserve would be touched.

The v1 restore policy should be move-only:

```text
encrypted compressed blob -> restored page -> delete compressed blob
```

Keeping duplicate compressed and decompressed copies is forbidden in v1 unless a
future design proves the complexity and memory overhead are necessary.

Required invariant:

```text
background decompression must never be the cause of renewed memory pressure
```

---

## 13. Thrash detection

`ramzip` must detect when compression is no longer helping.

Thrash indicators include:

- high rate of compress/fault-in/recompress cycles for the same task;
- compressed pages faulting soon after compression;
- CPU time spent compressing or decrypting pages exceeding a budget;
- hard cap reached while free memory still falls;
- foreground latency degradation caused by memory churn;
- per-task contribution exceeding a fairness threshold.

When thrash is detected, the VM must reduce speculative compression, stop
background warm-up, and escalate to the next pressure policy: optional lower
swap if approved, task freeze, task kill, or OOM. It must not spin.

---

## 14. Interaction with scheduler and desktop

The scheduler should expose only the information the VM needs and no more.
Avoid interface creep.

Useful scheduling signals may include:

- foreground or interactive task class;
- realtime or latency-critical task class;
- task recently resumed;
- task currently faulting heavily;
- CPU pressure;
- thermal or battery pressure if such signals already exist.

If these signals do not exist, the implementing agent must either avoid using
them in that stage or add them as a complete, tested, documented interface with
current callers. Do not add speculative public methods.

Desktop-specific policy must not leak into the kernel as GUI knowledge. The VM
can consume generic task classes or pressure hints. The desktop, compositor, and
taskbar remain optional session frontends; headless RustOS must keep the same
memory-safety behaviour.

---

## 15. Interaction with optional disk swap

Disk swap is explicitly lower priority than `ramzip`.

If a later stage implements block swap:

- it must be encrypted independently;
- it must not rely on filesystem encryption alone unless the swap object is a
  first-class encrypted RustOS object with approved nonce and key handling;
- its backing store must be selected by durable identity through the
  `ALIAS.md`/`DRIVES.md` namespaces — a pinned `disk:`/`part:`/`vol:` alias
  with a fingerprint, or an `id::` root — never by a discovery-order name
  (`disk:0`, `dev:sda`), a self-reported label, or `/Storage` ordering, and
  never by a board or SoC name;
- resolving the backing store yields a capability, not a pathname, and is
  capability-gated and fail-closed; it must not depend on the single `/`
  root view, so an otherwise-healthy swap volume stays openable via `id::`
  even when the System volume or machine alias policy is unavailable;
- removable backing must carry the `ALIAS.md` generation checks: a handle
  goes stale (`StaleGeneration`) on media removal or replacement, and the
  swap path then fails closed rather than writing to the wrong device;
- it must fail closed if the backing device disappears;
- swap to removable media (the SD card on a Raspberry Pi-class board is the
  motivating case) is disabled by default; "removable" is the discovered
  device property reported through hardware-tree/device-manager discovery,
  not a board name baked into generic code, and enabling it requires a
  reviewed emergency-only policy;
- `ramzip` must remain usable if block swap is unavailable;
- swap metadata and diagnostics must not expose plaintext page contents.

A missing or failed lower-tier swap path must not corrupt the RAM tier.

---

## 16. Diagnostics and ABI policy

The first implementation should avoid adding public ABI.

Internal counters may be logged through existing `lib/log` event paths and
exposed only through existing, capability-checked diagnostics if those already
support memory statistics.

If a public query is required later, it belongs in the existing System
Information API or an approved successor, not in `/proc`, `/sys`, a text scrape
file, or an ad-hoc syscall. Adding it requires:

- `lib/abi` type updates;
- generated ABI drift checks;
- capability policy;
- rustdoc;
- mdBook docs;
- fuzz tests for the decoder;
- `PLAN.md` update;
- full workspace validation.

No user setting may weaken encryption, authentication, fail-closed behaviour,
reserve preservation, or security logging.

---

## 17. Staged implementation plan

Each stage below must land as a complete, tested, documented, fully-gated
change. Do not merge stubs or partial plumbing. Do not add public interfaces
before a current in-tree caller consumes them.

### SWAP0 - Planning and docs only

Deliverables:

- Place this document under `plans/SWAPSWAPSWAP.md`, or another approved
  location after `AGENTS.md` section 3 is updated.
- Add a concise reference from `PLAN.md` only if this work is accepted into the
  staged roadmap.
- Do not add code in SWAP0.

Tests:

- Documentation build and spec-review checks required by the repository.

### SWAP1 - Metadata, eligibility, and accounting model

Deliverables:

- Internal `kernel/mem` model for compressed-entry metadata.
- Eligibility classifier for anonymous page candidates.
- Per-address-space or per-task accounting model.
- Min/soft/hard cap accounting.
- Reserve accounting with typed refusal reasons.
- No compression, encryption, or page-table integration yet unless the stage is
  explicitly expanded and fully tested.

Tests:

- Eligible and ineligible page classes.
- Unknown page type rejects.
- Pinned/DMA/device/kernel/sensitive/realtime pages reject.
- Cap limits and reserve preservation.
- Per-task accounting fairness.
- No `unwrap()`, `expect()`, `panic!()`, or `todo!()` in production paths.

Docs:

- `docs/src/architecture/memory.md` pressure-tier overview.

### SWAP2 - Encode/decode store

Deliverables:

- Compression through the approved RustOS compression abstraction.
- Encryption/authentication through `lib/crypto`.
- Bounded encrypted-blob store in RAM.
- Entry nonce allocation with uniqueness proof or model.
- Zeroization of temporary plaintext buffers.
- Corruption and authentication failure handling.

Tests:

- Round-trip page content.
- Incompressible-page rejection.
- Corrupt metadata rejection.
- Corrupt ciphertext authentication failure returns no plaintext.
- Nonce uniqueness across allocation/free/reuse patterns.
- Zero-on-free or zero-before-reuse for sensitive temporaries.
- Fuzz target for compressed-entry decode and metadata validation.

Docs:

- Crypto and compression rationale in memory docs.
- Security docs entry for authentication failures and audit/log behaviour.

### SWAP3 - VM pressure integration

Deliverables:

- Pressure-band integration with reclaim before compression.
- Cold anonymous page compression into `ramzip`.
- Compressed page fault-in path.
- Decompression reserve enforcement.
- OOM or pressure escalation when `ramzip` cannot help.
- No background decompression yet unless included completely.

Tests:

- Host-side pressure simulation.
- Reclaim-before-compress ordering.
- Fault-in restores exact page bytes.
- Hard cap reached escalates cleanly.
- Reserve cannot be consumed by compression.
- Repeated compress/fault cycles do not leak frames or metadata.
- QEMU memory-pressure integration test where practical.

Docs:

- Update memory architecture page with pressure bands and failure modes.

### SWAP4 - Fault clustering and opportunistic warm-up

Deliverables:

- Tiny fault clustering around recently faulted pages.
- Optional background warm worker with strict budgets.
- Hysteresis between compression and warm-up thresholds.
- Scheduler integration only through existing generic task classes or a fully
  approved new interface.
- Thrash detection and backoff.

Tests:

- Warm-up never runs under pressure.
- Warm-up stops immediately when pressure returns.
- Warm-up cannot touch reserves.
- Fault clustering helps only nearby eligible pages.
- Foreground task budget cannot starve background fairness.
- Thrash detection stops churn and escalates policy.

Docs:

- Update memory docs with latency policy and background worker constraints.

### SWAP5 - Optional lower-tier swap, if approved later

Deliverables:

- Separate design document for encrypted block/ARXFS swap, consistent with
  `ALIAS.md` and `DRIVES.md` storage naming.
- Capability, storage, and failure policy. The swap backing is named by
  durable identity (pinned `disk:`/`part:`/`vol:` alias with fingerprint, or
  `id::` root), resolution returns a capability rather than a pathname, and
  it does not depend on the single `/` root view.
- Removable-media default-off policy keyed on the discovered `removable`
  device property (the Raspberry Pi SD card being the motivating case), not
  on a board name in generic code, with `ALIAS.md` generation/staleness
  handling for the backing handle.
- No dependency on `ramzip` internals except priority ordering.

Tests:

- Device disappearance fails closed.
- Backing handle goes stale on media removal/replacement and fails closed.
- Discovery-order or label-only selection is rejected; durable identity is
  required.
- RAM tier remains correct without lower-tier swap.
- Lower-tier authentication failure returns no plaintext.

Docs:

- Storage and memory architecture updates.

---

## 18. Required test matrix summary

The completed feature must include tests for:

```text
eligibility:
  anonymous cold page accepts
  file cache reclaimed before anonymous compression
  pinned page rejects
  DMA page rejects
  device/MMIO page rejects
  kernel stack rejects
  page table rejects
  crypto-secret/sensitive page rejects
  realtime/latency-critical page rejects
  unknown page type rejects

accounting:
  min reserve is capacity, not eager stolen RAM
  soft cap limits normal pressure relief
  hard cap limits emergency growth
  metadata bytes accounted
  per-task contribution accounted
  emergency reserve preserved
  decompression reserve preserved
  no metadata leak on free

compression:
  round-trip known vectors
  random page property tests
  incompressible page rejected or explicitly accounted as raw if approved
  malformed compressed stream rejected
  excessive output length rejected
  decode never writes out of bounds

crypto:
  nonce uniqueness model or property test
  ciphertext corruption fails closed
  metadata tamper fails closed
  authentication failure returns no plaintext
  temporary plaintext zeroed before reuse
  key material never exposed through diagnostics

VM integration:
  pressure bands transition with hysteresis
  reclaim-before-compress ordering
  cold page compressed and faulted back
  concurrent faults on same entry are safe
  entry free racing fault-in is safe
  OOM is typed and non-panicking
  hard cap escalation is deterministic
  no spin under impossible pressure

background warm-up:
  no run under pressure
  no run below warm-up threshold
  stops on pressure transition
  respects CPU/page/byte budget
  preserves decompression reserve
  does not recreate pressure
  foreground hints are generic and optional

thrash:
  repeated recompress/fault cycle detected
  compression disabled or reduced for thrashing task
  escalation path selected deterministically
  no retry-until-it-works loop

observability:
  counters update consistently
  security-relevant failures log stable events
  diagnostics expose no plaintext
  public ABI absent unless approved and tested

optional lower-tier swap (if approved):
  backing selected only by durable identity, never discovery-order or label
  resolution returns a capability, not a pathname
  removable backing handle goes stale and fails closed on media change
  removable default-off keyed on discovered property, not a board name
```

---

## 19. Benchmarks and performance evidence

Implementation is incomplete without performance evidence.

Required benchmark areas:

- compression latency per page;
- decompression latency per page;
- fault-in latency with and without clustering;
- CPU cost under moderate pressure;
- CPU cost under severe pressure;
- memory saved per workload class;
- interactive latency with foreground tasks;
- Raspberry Pi-class small RAM profile;
- desktop/laptop larger RAM profile;
- wasm target behaviour if the native VM model is represented there;
- worst-case incompressible workload;
- thrashing workload.

Benchmarks must report estimates as estimates, not guarantees. Any default cap
or threshold chosen because of benchmark data must cite the benchmark in docs or
completion notes.

---

## 20. Acceptance checklist

This work is complete only when all applicable items are true:

- `AGENTS.md` has been read and remains the superior contract.
- `plans/ALIAS.md` and `plans/DRIVES.md` have been read, and any storage
  this work names (the optional lower-tier swap) uses their durable-identity
  naming, never discovery-order, label-only, or board-specific names.
- `PLAN.md` has been updated if a staged roadmap item advanced.
- The implementation is Rust-only.
- There is no C, C++, or new assembly.
- No public ABI was added unless a current caller required it and the ABI work
  was fully documented, tested, and drift-checked.
- No user knob disables encryption, authentication, fail-closed behaviour, or
  reserve preservation.
- Compression happens before encryption.
- All encrypted blobs are authenticated unless a reviewed benchmark-backed
  exception is accepted.
- Authentication failure returns no plaintext.
- Temporary plaintext is zeroed before reuse or free.
- `ramzip` starts with near-zero idle cost.
- The minimum capacity guarantee is not implemented as eager permanent RAM loss.
- Emergency reserves and decompression reserves are hard invariants.
- Ineligible page classes fail closed.
- Background decompression is opportunistic, budgeted, and unable to recreate
  pressure.
- Thrash detection exists and stops churn.
- No production `unwrap()`, `expect()`, `panic!()`, `todo!()`, ignored test, or
  retry-until-it-works loop was introduced.
- Any `unsafe` is justified, encapsulated, and tested.
- Unit tests, integration tests, fuzz/property tests, docs, and benchmarks land
  with the code.
- The whole-project gate has been run in the foreground to completion:
  `cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
  `cargo xtask fuzz --secs 5`, and any other `.github/workflows/ci.yml` command
  not covered by those commands, including the developer-machine soak cap where
  applicable.
- The completion report quotes the actual command output.

---

## 21. Prompt for an implementation agent

Use this prompt when RustOS is ready to implement the first stage:

```text
You are implementing the next approved stage of `plans/SWAPSWAPSWAP.md` for
RustOS.

Before coding, read `AGENTS.md`, `PLAN.md`, `plans/SWAPSWAPSWAP.md`,
`plans/ALIAS.md`, `plans/DRIVES.md`, `docs/src/architecture/memory.md`,
`docs/src/architecture/security.md`, `kernel/mem`, `kernel/sched`,
`kernel/sec`, `kernel/core`, `lib/crypto`, `lib/log`, and any existing VM
pressure, page-reclaim, OOM, compression, logging, or diagnostics code.

State the assumptions you verified from the repository: current page-state
model, frame allocator, page-table abstraction, page-fault path, reclaim/OOM
path, scheduler pressure hooks, sensitive-memory handling, crypto wrapper
surface, logging/audit event ranges, and whether any compression abstraction
already exists.

Implement only the approved stage. Do not add stubs, `todo!()`, ignored tests,
`#[allow(...)]` silencing, speculative public ABI, public syscalls, public
capabilities, disk swap, hibernation, `/proc`, `/sys`, C code, C++ code, or
hand-written assembly. Do not add a new dependency unless the AGENTS dependency
policy, PLAN.md, workspace, docs, license/advisory audit, and tests are updated
in the same change.

The design target is a near-zero-idle-cost encrypted compressed anonymous-memory
tier. Compression happens before encryption. The tier starts under memory
pressure after cheaper reclaim, preserves emergency and decompression reserves,
fails closed on corruption, authenticates encrypted entries, zeroes temporary
plaintext, avoids ineligible pages, detects thrash, and never lets background
warm-up recreate pressure. If you implement the optional lower-tier swap, name
its backing only by durable identity through `plans/ALIAS.md`/`plans/DRIVES.md`
(pinned `disk:`/`part:`/`vol:` alias or `id::` root), resolve it to a capability
rather than a pathname, and keep removable backing default-off with stale-handle
fail-closed behaviour.

Finish by running the full workspace gate in the foreground and waiting for it
to exit: `cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
`cargo xtask fuzz --secs 5`, and anything else `.github/workflows/ci.yml` runs
that those commands do not already cover. On a developer machine, any soak run
must use the AGENTS.md developer cap. Quote actual command output and state the
AGENTS.md verdict.
```
