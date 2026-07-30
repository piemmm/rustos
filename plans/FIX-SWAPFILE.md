# FIX-SWAPFILE.md - Encrypted compressed partition swap (SWAP5)

Status: **planned.** This is the binding design for the optional lower-tier
block swap that `plans/SWAPSWAPSWAP.md` scopes as its SWAP5 future stage
(`plans/SWAPSWAPSWAP.md` §3, §15, §17 SWAP5). It supersedes the one-paragraph
SWAP5 sketch there: SWAP5 in `SWAPSWAPSWAP.md` now points here for the full
design, and this file owns the block-swap tier end to end.

`ramzip` (SWAP1–SWAP4, the encrypted compressed **RAM-resident** anonymous
tier) is complete and switched on. This document is the tier **below** it: an
encrypted, compressed, page-slotted swap area on a **dedicated raw block
partition**, activated and deactivated dynamically over one or more block
devices. It is the last tier before task freeze/kill/OOM.

Target: TAIRiX
Primary code area: `kernel/mem` (the swap tier, slot maps, demotion path)
Secondary code areas: `kernel/sec` (capability + quota enforcement),
`kernel/core` (pressure escalation, page-fault-in), `kernel/syscall`
(`swapon`/`swapoff` ABI), `lib/crypto`, `lib/log`, `lib/abi` (the
`swapon`/`swapoff` + rlimit surface, only with their in-tree callers),
the storage stack (`lib/partition`, block-device capabilities, `plans/DRIVES.md`
/ `plans/ALIAS.md` durable identity), and `userland/system/installer` (lay out
the swap partition in the secure default).
Repository placement: `plans/FIX-SWAPFILE.md`, referenced by the AGENTS.md
§15.18 jump-sheet row for "Memory pressure, reclaimable memory, swap tiers".

This is a design, invariant, staging, and acceptance document, not a changelog
and not merge-ready code. If it conflicts with `AGENTS.md`, `AGENTS.md` wins.
Then `plans/ALIAS.md` / `plans/DRIVES.md` for storage identity, then
`plans/SWAPSWAPSWAP.md` for the RAM tier it sits under, then this file. If it
conflicts with the repository's actual memory manager, scheduler, security,
storage, or ABI state, the implementing agent must surface the mismatch before
implementing a guess (§15.7).

---

## 1. Source of truth

Before touching code, the implementing agent must read and reconcile:

1. `AGENTS.md` (binding; §2, §4, §5, §11, §16, §18, §19, §24, §26 especially).
2. `PLAN.md`.
3. `plans/SWAPSWAPSWAP.md` (the `ramzip` RAM tier this sits below; SWAP5).
4. `plans/SMARTRAM.md` (reclaimable-cache tier; the cheapest reclaim, above
   `ramzip` and swap in the escalation order).
5. `plans/ALIAS.md` (resource alias/selector namespaces: `disk:`, `part:`,
   `vol:`, fingerprints, pinned aliases, generation/staleness checks).
6. `plans/DRIVES.md` (durable storage identity `id::`, the storage-root forest,
   the rule that a healthy volume stays openable without the single `/` view).
7. `docs/src/architecture/memory.md` (pressure bands, tiering, reserves).
8. `docs/src/architecture/security.md`.
9. `docs/src/filesystem/drives.md` (binding storage-namespace spec).
10. `kernel/mem` (`ramzip`, `seal`, `coldscan`, pressure, frame allocator,
    reserves, per-task accounting), `kernel/mem::seal` (the shared sealing
    primitive: `SealKey`, `EntropySource`, `NonceSequence`).
11. `kernel/sec`, `kernel/core`, `kernel/syscall`, `lib/crypto`, `lib/compress`,
    `lib/log`, `lib/partition`, and the block-device driver/capability surface.
12. Any existing resource-limit (`rlimit`/`ulimit`) facility, or its absence
    (§24.3 — it may need building as part of the quota stage).

TAIRiX rules that matter especially for this work:

- Rust only; no C, C++, hand-written assembly, or generated headers added here.
- Security and correctness are the floor; performance is first-class inside it.
- No public ABI, syscall, capability, or userland service without a current
  in-tree caller and the ABI/docs/tests/`PLAN.md` landing in the same gated
  change.
- No duplication of constants, pressure thresholds, page-state algebra,
  compression/codec logic, crypto nonce logic, or accounting across tiers or
  architecture backends. The `ramzip` codec and `seal` primitive are **shared**,
  not re-implemented.
- No `unwrap()`/`expect()`/`panic!()`/`todo!()`, ignored tests, sleep/spin
  loops, retry-until-it-works, global mutable statics, or commented-out tests in
  production paths.
- `unsafe` carries a `// SAFETY:` block, is encapsulated behind a safe API, and
  is covered by a test or model check.
- Every stage ships rustdoc, mdBook docs, unit + integration + fuzz/property
  tests, performance/wear evidence, and the whole-project validation gate.

---

## 2. Goal

Provide an encrypted, compressed, page-slotted swap tier on a **dedicated raw
block partition**, sitting one level below `ramzip`, so a machine under
sustained memory pressure degrades gracefully instead of collapsing — while
staying strictly **better than Linux**: encrypted, compressed, integrity-checked
and fail-closed *by default*, where Linux gives you plaintext, unchecked,
silently-corrupting swap unless you hand-configure `dm-crypt`.

The tiering model (unchanged from `SWAPSWAPSWAP.md` §2, this document fills the
"optional block swap" line):

```text
active RAM
  -> SMARTRAM reclaimable cache reclaim        (cheapest; rebuild from source)
  -> ramzip encrypted compressed RAM tier      (SWAPSWAPSWAP.md)
  -> encrypted compressed PARTITION swap        (THIS DOCUMENT)
  -> pressure policy: freeze, kill, or OOM cleanly
```

This tier is the **last tier before freeze/kill/OOM**. It is never the first
pressure sink, never mandatory, and `ramzip` must remain fully usable with no
swap partition present (`SWAPSWAPSWAP.md` §15).

---

## 3. Why a dedicated raw partition, not a file in an ARXFS volume

This is a settled design decision, defensible under a Linus-style review for
four independent reasons. Swap is a **raw block range we own end to end**, given
a distinct swap partition type, never a file inside a filesystem volume.

1. **No double work (§2.16).** ARXFS already compresses every record, encrypts,
   checksums, and can dedup/FEC. A swap *file* inside ARXFS would compress every
   page twice, encrypt twice, checksum twice — pure waste on the single hottest
   reclaim path, and the flash-wear win is eaten by the outer layer's write
   amplification anyway.
2. **No semantic mismatch.** Swap needs a stable, predictable, page-slotted
   address space ("predictable latency over ratio", `SWAPSWAPSWAP.md` §8). A COW,
   snapshotting, extent-allocating, self-compacting filesystem is the opposite:
   its allocator would move blocks under us, its snapshots would pin dead swap
   pages forever, and its GC would fight our slot reuse. A raw partition gives
   direct sector-addressable slots with nothing in the way.
3. **No layering inversion (§17.4).** Swap is a `kernel/mem` concern that must
   work *before and below* a healthy filesystem (pressure can hit during early
   boot, during FS repair). Making the memory subsystem depend on the filesystem
   for its pressure-relief backing is a dependency direction to reject on sight.
4. **Cleaner fail-closed story (§5.4).** A raw partition has one authority path
   (a capability to that block range) and one integrity model (our AEAD tag). A
   file drags the whole filesystem's permission/ACL/allocation surface between
   us and the sectors.

---

## 4. Durability: no redundancy, but mandatory integrity detection

Swap is **ephemeral by construction**. The seal key is an ephemeral per-boot
random key that is never persisted and is discarded on shutdown (§4), so nothing
in the swap area is even *decryptable* after a reboot. Building FEC, replicated
copies, repair, or scrub to survive reboots for data that is cryptographically
dead on reboot is effort defending a property we deliberately do not have.

The line that must survive review — **detection is not redundancy**:

- **Drop recovery redundancy:** no FEC, no mirrored copies, no repair, no scrub.
  Correct — swap dies with the boot.
- **Keep integrity detection, and it is free:** the per-record **AEAD tag
  already authenticates every page**. A torn write, a bit-flip, or a failing
  disk (§26.5) produces an authentication failure.
- **Auth failure returns no plaintext and fails closed (§5.4, §2.17).** We never
  silently fault a corrupt page back into a process's address space.
- **On detected corruption there is no repair to attempt** (that is the
  redundancy we removed), so the deterministic outcome is: the fault propagates
  to the owning task, the task is killed with a **stated reason** (§24.1
  fail-loud), and the event is recorded on the hash-chained audit log (§19.4).
  This is strictly better than Linux, where a swap read error can silently
  corrupt a process.

The charter phrasing to carry into acceptance: **"no durability redundancy
(swap dies with the boot), but mandatory integrity detection via the AEAD tag;
detected corruption fails closed to a killed task, never a silently-served
page."** Cutting redundancy is fine; cutting detection is a §2.17 security
regression and forbidden.

---

## 5. Compress-once, re-seal: the `ramzip → partition` demotion path

The expensive work is **compression** (LZ4-class via `lib/compress`), not
encryption (ChaCha20-Poly1305-class via `lib/crypto` is cheap). So demotion
reuses the *compressed form* and only redoes the seal — compression happens
**once** across both tiers, which is the flash-wear / CPU win:

```text
ramzip blob (compressed, sealed under ramzip's SealKey)
  -> unseal with ramzip key           (cheap; yields compressed-plaintext in RAM)
  -> re-seal that compressed buffer under the SWAP tier's OWN SealKey + nonce + AAD
  -> write the (short) compressed ciphertext to the swap partition slot
  -> zero the transient compressed-plaintext buffer before it leaves scope
```

Binding rules for this path:

- **Compression is never repeated.** We write compressed bytes to disk; we never
  recompress. (Wear/CPU goal met.)
- **Keys and metadata stay independent (`SWAPSWAPSWAP.md` §3, §15;
  `seal.rs` docs).** The swap tier holds its **own** `SealKey` and
  `NonceSequence`, distinct from `ramzip`'s. The demotion path does **not** reuse
  `ramzip`'s key, nonce, or on-blob metadata/AAD format. This is a hard rule: an
  at-rest device compromise must never hand the attacker the live RAM tier's
  key, and vice-versa. Re-dumping `ramzip`'s sealed bytes straight to disk is
  **forbidden** for exactly this reason.
- **The swap slot's AAD binds the swap slot identity** (area id, slot index,
  epoch/generation), not the `ramzip` entry identity.
- **Incompressible input is the swap tier's own first-class case.** Pages
  `ramzip` refused as incompressible are valid swap input. The swap tier tries
  to compress and — unlike `ramzip` — **may store raw as its last-resort tier**
  (disk is the bottom), accounted as an expensive (full-page) entry in the slot
  map and the quota ledger.
- **Shared codec (§2.2).** The "compress→seal / unseal→decompress" codec used by
  `ramzip` is factored into one shared place (`kernel/mem`, built on
  `lib/compress` + `kernel/mem::seal`) and called by both tiers with their own
  key. Neither tier carries a private copy.

---

## 6. On-device record shape: page-slotted with short compressed writes (v1)

Compressed blobs are variable length, forcing a layout choice.

- **v1 — page-slotted device, short compressed writes (chosen).** One
  page-sized slot per swappable page; write only the compressed length (plus the
  AEAD nonce+tag header), padded/aligned to the device sector. Simple, no
  fragmentation, trivially fail-closed, and it still delivers the flash-wear win
  (write amplification is what wears flash, and we write fewer bytes per page).
  It does **not** reclaim disk *space* from compression — accepted for v1.
  Matches `SWAPSWAPSWAP.md` §8 "predictable latency over ratio", granularity one
  page, huge regions forbidden in v1.
- **Deferred — extent-packed device.** Packs blobs to reclaim disk space too,
  but needs an on-device allocator, compaction/GC, and fragmentation handling.
  Real complexity, justified with measurements later, not v1. Do not build it
  speculatively (§2.4).

On-device slot record (v1): `nonce ‖ tag ‖ compressed-or-raw payload`, the
payload length carried in the in-RAM slot-map entry (never trusted from the
device — the slot map is authoritative and its figures, not a length recomputed
from a corruptible blob, drive ledger releases). The swap-area header identifies
the swap partition type and geometry; it carries **no key material and no
plaintext**, and nothing in it is trusted to widen bounds without validation
(§5.4). Because the key is ephemeral per boot, a stale header from a previous
boot is simply undecryptable and is re-initialised on activation.

---

## 7. Multiple swap areas, per-device speed/priority selection (§26.1)

Each activated partition is its **own independent tier instance**: its own slot
map, its own `EncryptedSwap`/`SealKey`/`NonceSequence`, its own accounting. A
machine may have several at once (e.g. an NVMe area and a SATA area).

- **Per-device, not global (§26.1).** Reclaim picks among areas by
  **priority + speed class discovered per device** (never let a slow spinning
  disk stall or steal bandwidth from a fast NVMe area). Speed class comes from
  the hardware-tree/device discovery, never a board name in generic code (§2.20).
- **Fair across users and devices (§26.2).** One tenant's bulk sequential swap
  traffic cannot starve another's latency-sensitive access; per-`uid` fairness
  and per-device bounds hold under combined load.
- **Waiters block on completion, never busy-poll (§2.23).** A page-out/page-in
  awaiting device completion parks and is woken by the device interrupt; no tight
  poll loop, no core pegged at 100%.
- **Bounded work, fail closed (§24.3, §5.4).** Per-area queue occupancy and
  in-flight I/O are bounded; exhaustion fails closed as a typed error rather than
  letting an unbounded request fan-out exhaust kernel memory (§4).

---

## 8. Backing selection by durable identity only (§16.1, ALIAS/DRIVES)

The block range a swap area lives on is named by **durable identity**, never by
ordering or a self-reported label:

- **Accepted:** an `id::<volume-id>` root, or a pinned `disk:`/`part:`/`vol:`
  alias carrying a fingerprint (`plans/ALIAS.md`, `plans/DRIVES.md`).
- **Refused:** `disk:0`, `dev:sda`, `/Storage` ordering, a self-reported label,
  a board/SoC name, or any discovery-order name. These are rejected at
  activation, not silently coerced.
- **Resolution yields a capability to the block range, not a pathname (§5.4).**
  It is capability-gated and fail-closed, and it must **not** depend on the
  single `/` root view: an otherwise-healthy swap area stays activatable via
  `id::` even when the System volume or machine alias policy is unavailable
  (`plans/DRIVES.md`).
- **Generation/staleness (`plans/ALIAS.md`).** A removable or replaceable
  backing handle goes `StaleGeneration` on media change; the swap path then
  **fails closed rather than writing to the wrong device**. Device disappearance
  fails closed too.
- **No plaintext in diagnostics.** Swap metadata and diagnostics never expose
  plaintext page contents or key material (§16, §19.4).

---

## 9. A self-declaring swap partition type + installer default (§11, §18)

Give the swap partition a distinct TAIRiX GPT partition-type GUID (our own), so:

- **Discovery (§18) recognises a swap area structurally**, by partition type,
  without a label or heuristic — never by a board name in generic code (§2.20);
  "removable" and speed class come from the discovered device properties.
- **The installer (§11) lays one out in the secure default:** encrypted-swap
  only (the `EncryptedSwap` type makes plaintext swap unrepresentable, §4),
  ephemeral-keyed, **no passphrase, nothing recoverable at rest** — exactly as
  §4/§11 already mandate. Expert mode may not create a plaintext swap
  (§11). A headless image (§17.3) behaves identically.
- **The swap-area header carries no secret** (see §6): the partition type and
  geometry are public; the key never touches disk.

---

## 10. `swapon` / `swapoff` over multiple block devices

A Linux-style dynamic activate/deactivate, done more safely.

- **`swapon <durable-id> [priority]`** activates a swap area: resolve the
  durable identity (§8) to a block-range capability, initialise the area's
  in-RAM slot map and a fresh ephemeral `SealKey`, register it in the reclaim
  set at its priority/speed class (§7). Idempotent; re-activating an active area
  is a typed no-op error, not a double-register.
- **`swapoff <durable-id>`** deactivates — but **drains, never abandons** (this
  is where we beat Linux's sharp edge; see §11).
- **Capability-gated and audited (§5.2, §5.4, §19.4).** Activating/deactivating
  swap is a privileged, logged operation over a block device — structurally a
  mount-policy decision, so it reuses the existing mount-class authority
  (`CAP_FS_MOUNT` or a sibling). **Do not mint a new capability speculatively**
  (§5.2): decide the exact capability when the enforcement point lands, with a
  live holder, not before.
- **Removable default-off (§26, keyed on the discovered property).** A
  `removable` device (the Pi SD card, a USB stick) is **never** a `swapon`
  target unless a reviewed emergency-only policy opts in. This is the whole
  reason `ramzip` exists first — protect fragile flash. "removable" is the
  discovered device property, never a board name in generic code (§2.20).
- **The `swapon`/`swapoff` ABI** lives in `lib/abi` under the syscall discipline
  (§9): versioned, hashed, frozen on the first release — added only with its
  in-tree caller (the `swapon`/`swapoff` command apps in `userland/shell/`,
  coreutils-shaped where a counterpart exists, §16.7).

---

## 11. Draining `swapoff` and forced hot-removal (better than Linux)

Deactivation cannot just forget an area — it must **migrate every live page off
it first**:

- **`swapoff` drains.** For each occupied slot: fault the page back into RAM, or
  promote it into another swap area / `ramzip`, then release the block-range
  capability. Draining is **incremental, interruptible, and parks on completion**
  rather than busy-spinning (§2.23, §26.6).
- **Nowhere to put the pages → fail closed.** If there is no RAM and no other
  area to hold the drained pages, `swapoff` **fails with a typed error** rather
  than losing data or hanging.
- **Hot-removal is the same drain path, forced.** When a swap device disappears
  or its handle goes stale (§8), the area is force-drained; pages that cannot be
  recovered fault their owning tasks **deterministically with a stated reason**
  (§24.1) — never a silent hang, never corruption, never a silently-served stale
  page.
- **Audit (§19.4).** Activation, deactivation, drain start/finish, forced
  hot-removal, and every fail-closed task fault emit stable audit events.

---

## 12. Runaway / slashdot / DoS defences — the "better than Linux" core

This tier must extend TAIRiX's existing per-principal fairness, never open a
global swap free-for-all. Each named threat from the design conversation has a
concrete answer:

- **Encrypted + compressed by default.** Linux swaps plaintext unless you
  configure `dm-crypt`; TAIRiX makes encrypted swap *unrepresentable-otherwise*
  (the `EncryptedSwap` type gate, §4) and compresses on top. Better on privacy
  *and* wear.
- **Runaway process consuming all RAM → the offender pays (§24.3, §26.2).**
  Per-user and per-task **swap quotas** via the §24.3 `rlimit`-equivalent,
  fail-closed. A runaway process hits *its own* ceiling and is OOM-killed in its
  own accounting; it cannot externalise cost onto the machine. `ramzip` already
  charges a per-task fair share (`task_share = band_cap/2`); the partition tier
  charges the **same per-principal ledger**, extended across areas.
- **Local/remote DoS resource-consumption → bounded and fail-closed.** Per-user
  and per-process swap footprint, per-area queue occupancy, and in-flight I/O are
  all bounded (§7, §24.3) and deny as typed errors when a limit is reached — a
  flood cannot exhaust kernel memory (§4) or the swap area.
- **Slashdot effect (valid load from many clients) → fair-share degradation
  (§26.2, §26.4).** When many legitimate clients push memory up, per-`uid`
  fairness means busy services get *paged, gracefully*, rather than one tenant's
  growth killing another. Pair with admission control / backpressure on the
  serving path (§26.4) so the machine degrades instead of collapsing.
- **Not getting in the user's way.** Swap is the *last* tier before
  freeze/kill/OOM: cheap SMARTRAM cache reclaim first, then `ramzip`, then
  partition swap (§2). Foreground/interactive and latency-critical pages are
  ineligible or last-evicted (`SWAPSWAPSWAP.md` §5, §11); foreground **direct
  reclaim** charges the cost to the task *driving* pressure, not its victim.
  Tickless throughout (§17.1).
- **Deterministic escalation, not a swap death-spiral.** `ramzip`'s existing
  thrash detection (`SWAPSWAPSWAP.md` §13) governs when demotion to disk helps
  vs. when to escalate to freeze/kill/OOM — deterministic, not a heuristic OOM
  killer picking the wrong process. Swap thrash (page faulting back in almost as
  fast as it is written out) is detected and escalates; it never spins (§2.1).

---

## 13. Pressure escalation and reserves

- **Escalation order (extends `SWAPSWAPSWAP.md` §6 critical band).** At critical
  pressure, after cheaper reclaim and `ramzip` are exhausted, the VM demotes cold
  `ramzip` entries to partition swap (compress-once re-seal, §5) and compresses
  further cold anonymous pages out through swap. Only when no eligible area can
  accept more does policy escalate to task freeze, task kill, or a clean typed
  OOM (`SWAPSWAPSWAP.md` §13; never a panic, §4/§2.9).
- **Reserves are hard invariants (`SWAPSWAPSWAP.md` §7).** Swap page-in needs
  free RAM to restore the page, so a **decompression/restore reserve** is held at
  all times; swap I/O has its own bounded reserve. Swap activity must **never**
  be the cause of reserve exhaustion — if honouring a reserve means refusing to
  page out, that is a normal typed refusal / escalation, not a panic.
- **Under memory pressure allocation still fails as a `Result` (§4, §26.3),**
  and swap-path buffers (compressed-plaintext staging, I/O buffers) are reclaimed
  before the system denies forward progress; no memory-pressure busy-loop
  (§2.23).

---

## 14. Diagnostics and ABI policy (§16, §16.6)

- Internal counters (areas active, per-area occupancy/priority/speed class,
  pages in/out, compressed vs raw entries, auth failures, drain progress,
  quota denials, forced-removal events) are logged through existing `lib/log`
  event paths with stable IDs (§19.4).
- Any user-facing query belongs in the **System Information API** (§16.6),
  behind the appropriate capability — never `/proc`, `/sys`, a text-scrape file,
  or an ad-hoc syscall (§16.1). Adding one requires `lib/abi` types, generated
  drift checks, capability policy, rustdoc, mdBook docs, a fuzz target for the
  decoder, a `PLAN.md` update, and full workspace validation.
- **No user setting may weaken** encryption, authentication, fail-closed
  behaviour, reserve preservation, the removable-default-off policy, quota
  enforcement, or security logging (§16, §2.17).
- Swap diagnostics never expose plaintext page contents or key material (§8).

---

## 15. Staged implementation plan

Each stage lands as a complete, tested, documented, fully-gated change. No
stubs, no `todo!()`, no no-ops, no speculative public interface before a current
in-tree caller consumes it. Each stage is landed complete or, if genuinely too
large, its landed part is complete and the remainder is surfaced (§2.19, §15.7).

`ramzip` is `kernel/mem::ramzip`; the shared sealing primitive is
`kernel/mem::seal` (`SealKey`/`NonceSequence`); the RAM tier's slot-addressed
encrypted-swap scaffold is `kernel/mem::swap` (`EncryptedSwap<B: SwapBackend>`),
which already makes plaintext swap unrepresentable (its constructor demands a
`SealKey`). These stages build the block backend under that gate.

### SF0 — this design document (planning only)

Deliverables:
- Place this document at `plans/FIX-SWAPFILE.md`.
- Add the AGENTS.md §15.18 jump-sheet row and a concise `PLAN.md` reference;
  repoint `plans/SWAPSWAPSWAP.md` §15 / SWAP5 here.
- No code in SF0.

Tests: the repository documentation-build and spec-review checks.

### SF1 — shared codec + block `SwapBackend` over a raw block range

Deliverables:
- Factor `ramzip`'s "compress→seal / unseal→decompress" codec into one shared
  `kernel/mem` codec (over `lib/compress` + `kernel/mem::seal`), and switch
  `ramzip` to it with no behaviour change (§2.2). No second copy remains.
- A block `SwapBackend` implementation for `EncryptedSwap`: page-slotted device
  layout (§6), backed by a block-range capability, with its **own** ephemeral
  `SealKey`/`NonceSequence` (independent of `ramzip`, §5).
- The in-RAM slot map (authoritative record lengths; free-slot allocation) and
  the swap-area header (public: type + geometry, no secret, validated on read).

Tests:
- Codec round-trip parity before/after the `ramzip` switch (no regression).
- Block backend round-trips a page (compressible, incompressible→raw, empty).
- Corrupt/torn ciphertext → auth failure → no plaintext (§4).
- Corrupt/oversized header or slot metadata → typed rejection, no OOB.
- Nonce uniqueness across allocate/free/reuse (model or property test).
- Zeroisation of compressed-plaintext staging buffers on every path.
- Fuzz target for the on-device record + header decoder.

Docs: `docs/src/architecture/memory.md` (the block tier + compress-once path);
`kernel/mem::swap` / codec rustdoc.

### SF2 — `swapon`/`swapoff` activation over durable identity

Deliverables:
- `swapon`/`swapoff` ABI in `lib/abi` (versioned, hashed) **with** its in-tree
  callers: the `swapon`/`swapoff` command apps in `userland/shell/` (§16.7).
- Durable-identity resolution (§8) → block-range capability; discovery-order /
  label / board-name selection refused.
- Multi-area registry with per-device priority/speed class (§7); idempotent
  activation; capability gate (mount-class, §10) and audit events (§19.4).
- Removable default-off keyed on the discovered property (§10).

Tests:
- Durable identity activates; discovery-order/label-only selection rejected.
- Device disappearance / `StaleGeneration` on media change → fail closed, no
  write to the wrong device.
- Removable device refused unless the emergency policy opts in.
- Capability gate denies without authority; every decision audited.
- Multiple areas register and order by priority/speed class.

Docs: memory + storage architecture pages; the command apps' `Help/` trees.

### SF3 — pressure integration: demotion, page-in, escalation

Deliverables:
- `ramzip → partition` demotion (compress-once re-seal, §5) driven from the
  critical pressure band (§13), gated on caps/reserves/quota.
- Compressed-page-fault-in from the swap area (move-only; discard the slot;
  fail closed with no plaintext on auth/decode failure).
- Deterministic escalation to freeze/kill/clean OOM when no area can accept
  more (§12, §13); swap-thrash detection reuses `ramzip`'s detector.
- Reserve enforcement: swap activity never causes reserve exhaustion (§13).

Tests:
- Demotion reuses the compressed form (no recompress), independent key/nonce.
- Fault-in restores exact page bytes + flags; auth failure faults the task
  with a stated reason and audit event, never a silent page.
- Hard-cap / no-area-available escalates cleanly (typed, non-panicking).
- Reserves cannot be consumed by page-out; no busy-loop under impossible
  pressure.
- QEMU memory-pressure integration test where practical.

Docs: memory architecture page (escalation order + failure modes).

### SF4 — per-user/per-task swap quotas via the §24.3 rlimit facility

Deliverables:
- If the §24.3 resource-limit (`rlimit`/`ulimit`) facility does not yet exist,
  build it completely as part of this stage (soft/hard bounds, inheritance,
  intersection on delegation, `CAP_RLIMIT_RAISE`-gated hard-bound raise,
  `ulimit` command app) — do not stub it (§2.19). If it exists, extend it with
  the swap dimension.
- A per-principal swap ledger (shared vocabulary with `ramzip`'s per-task
  share, §12) enforced fail-closed across all areas.
- Live usage + effective limits observable through the System Information API
  (§16.6), never `/proc`.

Tests:
- Runaway task hits its own ceiling → clean OOM in its own accounting, machine
  unharmed.
- Per-user/per-process footprint bounded; over-limit denied as typed error.
- Soft/hard bound semantics; hard-bound raise requires the capability.
- Inheritance + intersection across spawn/delegation.
- Fairness: one tenant cannot starve another of swap bandwidth (§26.2).

Docs: memory + security architecture; `ulimit` `Help/` + `docs/src`.

### SF5 — draining `swapoff` and forced hot-removal

Deliverables:
- Draining `swapoff` (§11): migrate every live page off before release;
  incremental, interruptible, parks on completion; fail closed with a typed
  error when there is nowhere to put the pages.
- Forced hot-removal via the same drain path; unrecoverable pages fault their
  owning tasks deterministically with a stated reason (§24.1) + audit.

Tests:
- `swapoff` drains all live pages then releases the capability.
- `swapoff` with no target RAM/area → typed failure, no data loss, no hang.
- Forced hot-removal recovers what it can and faults the rest deterministically
  with an audit event; no silent hang or corruption.
- Drain is interruptible and parks (no busy-spin).

Docs: memory + storage architecture (drain semantics, hot-removal).

### SF6 — installer swap partition + self-declaring type

Deliverables:
- The TAIRiX swap GPT partition-type GUID; discovery recognises a swap area by
  type (§9), never a label/board name.
- The installer lays out an encrypted, ephemeral-keyed swap partition in the
  secure default (§11); expert mode may not create plaintext swap; headless
  behaves identically.

Tests:
- Installer default lays out exactly one encrypted swap partition, no plaintext.
- Discovery recognises the swap type; a non-swap-typed partition is never used.
- Expert mode refuses plaintext swap.

Docs: installer + storage architecture pages.

---

## 16. Required test matrix summary

```text
backend/codec:
  shared codec round-trip parity (ramzip unchanged after the switch)
  page round-trip: compressible, incompressible→raw, empty
  torn/corrupt ciphertext fails closed, returns no plaintext
  corrupt/oversized header or slot metadata rejected, no OOB
  nonce uniqueness across allocate/free/reuse
  compressed-plaintext staging zeroed on every path
  independent key/nonce/metadata vs ramzip (no shared key path exists)
  decode/header fuzz target

activation (swapon/swapoff):
  durable id (id::/pinned-alias-with-fingerprint) activates
  discovery-order / label-only / board-name selection rejected
  resolution yields a capability, not a pathname
  independent of the single / root view (activatable when System vol down)
  StaleGeneration on media change fails closed (no wrong-device write)
  device disappearance fails closed
  removable default-off; emergency opt-in only
  capability gate + audit on every activate/deactivate
  multiple areas ordered by priority/speed class

pressure/demotion:
  ramzip->partition demotion reuses compressed form (no recompress)
  fault-in restores exact bytes + flags
  auth failure faults owning task with stated reason + audit, no plaintext
  hard-cap / no-area escalates to freeze/kill/clean OOM (typed, no panic)
  reserves never consumed by page-out
  swap thrash detected, escalates, never spins
  no busy-loop under impossible pressure

quotas (rlimit):
  runaway task OOM-killed in its own accounting; machine unharmed
  per-user/per-process footprint bounded, over-limit denied (typed)
  soft/hard bound semantics; hard-bound raise gated by capability
  inheritance + intersection across spawn/delegation
  fairness: one tenant cannot starve another's swap bandwidth

drain/hot-removal:
  swapoff drains all live pages then releases
  swapoff with nowhere to put pages: typed failure, no loss, no hang
  forced hot-removal recovers what it can, faults the rest deterministically
  drain is incremental, interruptible, parks (no busy-spin)

installer/discovery:
  secure default lays out one encrypted, ephemeral-keyed swap partition
  no plaintext swap in default or expert mode
  swap recognised by partition type, never label/board name
  headless identical

observability:
  counters update consistently; security failures log stable events
  diagnostics expose no plaintext or key material
  no public /proc, /sys, or ad-hoc syscall; SysInfo API only if a caller lands
```

---

## 17. Benchmarks, wear, and performance evidence (§19, §2.16)

Implementation is incomplete without evidence. Required areas:

- page-out latency (compress reused + re-seal + write) per page and per area;
- page-in latency (read + auth + decompress) per page and per area;
- demotion latency `ramzip → partition` (proving compression is not repeated);
- **write-amplification / bytes-written per page vs. an uncompressed baseline**
  — this is the SSD/NVMe/SD **wear** claim and must be measured, not asserted;
- CPU cost under moderate vs. severe pressure;
- per-device fairness: a slow area does not stall a fast area under mixed load;
- multi-area priority selection behaviour;
- worst-case incompressible workload (raw last-resort path);
- swap-thrash workload (detection + escalation, no spin);
- drain/`swapoff` throughput and interruptibility;
- the §26.7 combined floor: a ~1 GiB-RAM machine with several large swap areas
  active at once stays bounded, fail-closed, no panic, no busy-spin.

Report estimates as estimates. Any default priority, reserve, quota, or slot
constant chosen from benchmark data cites the benchmark in docs or completion
notes. Caps, watermarks, and quota defaults are implementation constants sized
from discovered hardware (§24.1/§24.2), never ABI, never a frozen scalar a
larger machine outgrows or a smaller one wastes.

---

## 18. Acceptance checklist

Complete only when all applicable items are true:

- `AGENTS.md` read and remains the superior contract; `plans/ALIAS.md`,
  `plans/DRIVES.md`, `plans/SWAPSWAPSWAP.md`, `plans/SMARTRAM.md` read.
- `PLAN.md` updated for the advanced stage; the §15.18 jump-sheet row present.
- Rust only; no C, C++, or new assembly; no hand-edited generated headers.
- No public ABI/syscall/capability/service added without a current in-tree
  caller and full ABI/docs/tests/drift/`PLAN.md` in the same gated change.
- Swap is a **dedicated raw partition**, never a file in an ARXFS volume.
- Encrypted-swap is unrepresentable-otherwise; **no plaintext swap** in any
  mode; the ephemeral per-boot key is never persisted.
- **No durability redundancy** (no FEC/mirror/repair/scrub); **mandatory AEAD
  integrity detection**; detected corruption fails closed to a killed task with
  a stated reason and an audit event — never a silently-served page.
- Compression happens **once** across both tiers (compress-once re-seal on
  demotion); compression before encryption; all blobs authenticated.
- Swap tier holds its **own** `SealKey`/`NonceSequence`/AAD; it never reuses
  `ramzip`'s key or metadata format.
- Backing selected by durable identity only (never ordering/label/board name);
  resolution yields a capability, not a pathname; independent of the `/` view.
- `swapon`/`swapoff` capability-gated and audited; removable default-off keyed
  on the discovered property; `swapoff` **drains** and fails closed when it
  cannot.
- Per-user/per-task swap quotas fail-closed through the §24.3 facility; a
  runaway pays its own cost; fairness holds under the slashdot/DoS cases.
- Reserves are hard invariants; swap never causes reserve exhaustion; OOM is a
  typed `Result`, never a panic; no busy-poll/spin anywhere (§2.23).
- No production `unwrap()`/`expect()`/`panic!()`/`todo!()`, ignored test, or
  retry-until-it-works loop; any `unsafe` justified, encapsulated, tested.
- Unit + integration + fuzz/property tests, docs, and wear/perf benchmarks land
  with the code; coverage targets (§7) met.
- The whole-project gate ran in the foreground to completion (`cargo fmt --all`,
  `cargo fmt --all --check`, `cargo xtask ci` once, `cargo xtask fuzz --secs 5`,
  and the `tools/ci/soak.sh` developer-cap soak); the completion report quotes
  the actual output and states the §23 verdict.

---

## 19. Prompt for an implementation agent

```text
You are implementing the next approved stage of `plans/FIX-SWAPFILE.md` for
TAIRiX (the encrypted compressed PARTITION swap tier, SWAP5, below `ramzip`).

Before coding, read `AGENTS.md`, `PLAN.md`, `plans/FIX-SWAPFILE.md`,
`plans/SWAPSWAPSWAP.md`, `plans/SMARTRAM.md`, `plans/ALIAS.md`,
`plans/DRIVES.md`, `docs/src/architecture/memory.md`,
`docs/src/architecture/security.md`, `docs/src/filesystem/drives.md`,
`kernel/mem` (`ramzip`, `seal`, `swap`, `coldscan`, pressure, reserves,
per-task accounting), `kernel/sec`, `kernel/core`, `kernel/syscall`,
`lib/crypto`, `lib/compress`, `lib/log`, `lib/partition`, the block-device
capability/driver surface, and any existing rlimit/ulimit facility.

State the assumptions you verified from the repository: the ramzip codec and
seal surface, the EncryptedSwap/SwapBackend scaffold, the block-device
capability model, durable-identity resolution (ALIAS/DRIVES), the pressure
band + escalation path, the audit event ranges, and whether a resource-limit
facility exists.

Implement only the approved stage completely. No stubs, todo!(), ignored
tests, #[allow(...)] silencing, speculative ABI/syscalls/capabilities, C, C++,
or hand-written assembly. No new dependency without the full AGENTS dependency
process in the same change.

Design invariants: dedicated raw partition (never a file in a volume);
encrypted-swap unrepresentable-otherwise with an ephemeral per-boot key; no
durability redundancy but mandatory AEAD integrity detection (corruption ->
killed task + audit, never a served page); compress once across both tiers
(compress-once re-seal on ramzip->partition demotion); the swap tier's own
SealKey/nonce/AAD, never ramzip's; backing named only by durable identity ->
capability, independent of the / view; removable default-off; swapon/swapoff
capability-gated and audited; swapoff drains and fails closed; per-user/task
swap quotas fail-closed via the rlimit facility; reserves are hard invariants;
no busy-poll/spin; typed OOM, never a panic.

Finish by running the full workspace gate in the foreground and waiting for it
to exit: `cargo fmt --all`, `cargo fmt --all --check`, `cargo xtask ci`,
`cargo xtask fuzz --secs 5`, and anything else `.github/workflows/ci.yml` runs
that those do not cover, with the developer-machine soak cap. Quote actual
command output and state the AGENTS.md verdict.
```
