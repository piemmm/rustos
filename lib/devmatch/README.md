# rustos-devmatch

Deterministic hardware-node ↔ driver bind-table match resolution for RustOS
(`lib/devmatch`, `AGENTS.md` §18.3 / §2.2).

A discovered hardware-tree node (`rustos_abi::HwNode`) carries the match keys
its discoverer emitted; a driver candidate carries the bind table its signed
manifest declared. This crate decides which driver binds which node:

- a bind-table entry matches a node when its `HwMatchKey` matches one of the
  node's keys (`HwMatchKey::matches` — exact for `compatible`/virtio,
  class-with-optional-vendor/device-wildcard for PCI/USB);
- when several drivers match the same node, the strictly highest matched bind
  priority wins;
- an unbroken tie across two *distinct* drivers at the highest priority is a
  packaging defect and the node is refused a binding — never a coin-flip
  (§2.1, §18.3).

## API

- `resolve(node_keys, candidates) -> MatchResolution` — `Winner` / `Tie` /
  `Unmatched`, order-independent and deterministic.
- `best_bind_priority(node_keys, bind_keys) -> Option<u16>` — the highest
  priority at which one candidate's table matches the node.
- `DriverCandidate { path, bind_keys }` — a candidate's logical image path plus
  its decoded bind table (the caller supplies it already fail-closed-decoded;
  this crate never re-parses image bytes).

## Why its own crate

The same match policy is reached from two strata the §17.4 layering keeps
apart: the user-space device manager (`userland/system/devmgr`) and the
kernel's interim in-kernel driver-candidate catalogue (`kernel/rustos-kernel`,
PLAN Stage 4.HW item 5). The kernel may not depend on a `userland/*` crate, so
the policy lives here in `lib/*` as the single definition both reach, never
duplicated (§2.2).

## Design

- `no_std`, `#![forbid(unsafe_code)]`, pure data comparison — no allocation, no
  I/O, no logging. Audit and the load mechanism stay with each consumer.
- Depends only on `rustos-abi` (the lowest layer): it compares ABI-owned wire
  types (`HwMatchKey`, `DriverBindKey`).

## Stability

Tier: `experimental`.
