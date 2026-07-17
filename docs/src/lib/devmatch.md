# `tairix-devmatch`

The deterministic **hardware-node ↔ driver bind-table match policy** for
TAIRiX (`AGENTS.md` §18.3). A discovered hardware-tree node
(`tairix_abi::HwNode`) carries the match keys its discoverer emitted; a driver
candidate carries the bind table its signed manifest declared. This crate
decides which driver binds which node, and nothing else — no allocation, no
I/O, no logging.

Stability tier: **experimental**.

## What it defines

| Item | Contents |
|------|----------|
| `resolve(node_keys, candidates)` | Resolve a node against every candidate → `MatchResolution`. |
| `best_bind_priority(node_keys, bind_keys)` | Highest priority at which one candidate's table matches the node. |
| `DriverCandidate { path, bind_keys }` | A candidate's logical image path plus its already-fail-closed-decoded bind table. |
| `MatchResolution` | `Winner { candidate, priority }` / `Tie { priority }` / `Unmatched`. |

## Rules (`AGENTS.md` §18.3)

- A bind-table entry matches a node when its `HwMatchKey` matches one of the
  node's keys (`HwMatchKey::matches`: exact for `compatible`/virtio,
  class-with-optional-vendor/device-wildcard for PCI/USB).
- The strictly highest matched bind priority wins.
- An unbroken tie across two **distinct** drivers at the highest priority is a
  packaging defect — the node is refused a binding, never a coin-flip (§2.1).
- A node matching nothing is `Unmatched`; the caller leaves it unbound and
  logged (§18.4), never a guess.

## Why its own crate

The same policy is reached from two strata the §17.4 layering keeps apart:

- the user-space device manager (`userland/system/devmgr`), the §18.3 autoload
  owner; and
- the kernel's interim in-kernel driver-candidate catalogue
  (`kernel/tairix-kernel::driver_catalog`, PLAN Stage 4.HW item 5).

The kernel may not depend on a `userland/*` crate (§17.4), so the policy lives
here in `lib/*` as the single definition both reach — never duplicated (§2.2).
It depends only on `tairix-abi` (the wire types it compares).
