# `rustos-devmgr` — device manager (driver autoload)

Stage 4.HW deliverable (`AGENTS.md` §18.3). The user-space service that
owns driver **autoload**: it matches every hardware-tree node against
the bind table each driver declares in its signed manifest and loads
the winners through the §8 driver-host load gate. Matching policy is
not kernel code (`AGENTS.md` §4 — microkernel-leaning).

## What this crate is

The **matcher** — the policy layer. It owns no load mechanism of its
own. `DeviceManager::autoload` walks the tree and, per non-root node,
fail-closed (`AGENTS.md` §5.4):

1. Resolve the node's match keys against every `DriverCandidate`'s
   bind table (`matcher::resolve`): the strictly highest matched bind
   `priority` wins; an unbroken tie between distinct candidates is a
   packaging defect and the node is refused a binding — never a
   coin-flip (`AGENTS.md` §18.3).
2. A node matching nothing is left **unbound and logged** — never an
   error and never a panic (`AGENTS.md` §18.4).
3. Load the winner through the injected `DriverLoader` once **per
   node** — each load spawns its own driver instance holding exactly
   that node's resource grants, so two identical devices (a virtio
   keyboard and a virtio mouse, say) each get a live instance rather
   than the second being bound in name only.
4. A load refusal fails only that node; the walk continues so one bad
   image cannot block boot.
5. Emit a `lib/log` audit record for every outcome.

## Seams

- `DriverCandidate` — a driver image's logical path plus its bind
  table, already decoded fail-closed by the load gate's
  `ParsedImage::decode_bind_table`. This crate never re-parses image
  bytes.
- `DriverLoader` — the load mechanism. The deployment integration
  point implements it over `rustos-drvhost`'s `Host::load` pipeline
  (signature verification, `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` gates,
  spawner hand-off), mapping `HostError` to `Errno` via `as_errno`.
  The device manager never inspects or bypasses the gate's checks.

## Audit events

Reserved `EventId` range `13000..14000`:

- `13001 NODE_BOUND` — node bound; fields `node`, `path`, `handle` (Info).
- `13002 NODE_UNBOUND` — no matching driver (Debug, §18.4 / §20): the
  routine, high-volume case (most nodes on a real device tree have no
  driver), filtered out by the default `Info` threshold so it never
  floods the slow diagnostic UART; lower the level to trace it.
- `13003 NODE_TIE_REJECTED` — unbroken priority tie refused; field
  `priority` (Warn).
- `13004 NODE_LOAD_FAILED` — load gate refused the winner; fields
  `path`, `errno` (Warn).

## Layering & safety

`no_std`, depends only on `rustos-abi`, `rustos-caps`, `rustos-log`,
and `rustos-util` (all `lib/*`), so the service never links a kernel,
driver, or other userland crate (`AGENTS.md` §17.4). No `unsafe`, no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).

## Stability

Tier: `experimental` (`AGENTS.md` §6). The wire formats consumed
(hardware-tree nodes, bind-table entries) are frozen by `rustos-abi`.

## Test surface

`cargo test -p rustos-devmgr` (16 unit tests): exact compatible and
numeric-key matching, multi-match priority resolution and order
independence, unbroken-tie rejection (and a tie broken by a higher
priority), no-match → unbound, root-node skip, capability-denied load
failing closed with the walk continuing, one-load-per-driver dedup,
and the `EventId` range/uniqueness invariants.

`cargo test -p rustos-drvhost --test devmgr_autoload` closes the loop
end-to-end: signed `.rxe` images with bind tables, decoded by the real
gate, matched here, and loaded through a real `Host` — including the
missing-`CAP_DRV_LOAD` refusal interleaving both subsystems' audit
records on one sink.

Docs: `docs/src/drivers/hardware-detection.md`.
