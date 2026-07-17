# FIXDRIVERS.md — Keep driver logic in `drivers/`, close the `lib/*` device-logic loophole

This is a staged plan for a charter amendment plus a multi-crate refactor. It
is **binding under `AGENTS.md`**; read `AGENTS.md` and `PLAN.md` first. Every
rule in both applies here without exception.

## 0. Problem statement (the intent being defended)

The intent of `AGENTS.md` is that **all device-driver logic lives in
`drivers/`**, never scattered into `lib/*`. Today `lib/vl805`, `lib/pcie_brcm`,
and the VL805/xHCI portions of their siblings sit in `lib/*`. That placement is
charter-legal *only* because two rules combine:

- **§2.20 device carve-out** — "a device's own driver/support crate may know
  its device."
- **§17.4 layering** — `drivers/* → lib/*` is one-way and `kernel/*` may never
  depend on `drivers/*`. So when device logic has a `kernel/*` consumer *and* a
  `drivers/*` consumer, it cannot live in `drivers/` (kernel can't depend on
  it) and must not be duplicated (§2.2), leaving `lib/*` as the only home.

The defect: for VL805/xHCI the `kernel/*` consumer is a **transitional
in-kernel keyboard bring-up scaffold** (`lib/vl805` README, "the transitional
in-kernel keyboard bring-up scaffold (`tairix-kernel`)"). A USB keyboard / xHCI
/ VL805 driver sits **above** the §18.6 bootstrap-floor (the driver store, not
below it), so per **§18.5** it "belongs in the store, discovered and loaded
into user space" — it is a defect, not a floor item. That illegitimate kernel
consumer is the *only* thing forcing the device logic out of `drivers/` into
`lib/*`. Remove it and the §17.4 justification evaporates.

`lib/vcmailbox` is the **legitimate survivor**: its kernel consumer is the
aarch64 **framebuffer boot console** (a genuine early-boot need), and its other
consumer is `drivers/display/rpi_hvs`. That is a true two-consumer split where
one consumer is a charter-legal non-driver. It stays in `lib/*`.

## 1. Decisions (binding for this plan)

- **A `lib/*` device-support crate is permitted only when a charter-legal
  non-driver consumer shares it.** A genuine `kernel/arch/<target>/`
  bootstrap-floor path (§18.6) or a driver of a *different* class qualifies. A
  **transitional in-kernel scaffold for a driver that §18.5 places in the
  user-space store does NOT qualify** — that scaffold is the defect to remove,
  not a justification for hoisting.
- **The minimal fix is the chosen path** unless the User elects the structural
  option in §4: remove the scaffold, make the keyboard a discovered user-space
  driver, and collapse single-consumer device crates back into `drivers/`.
- **No "for now"** (§2.19): the scaffold removal and the discovered-driver
  replacement land together; the keyboard is not left half-wired.

## 2. AGENTS.md amendment (close the loophole)

Tighten the §2.20 carve-out / §17.4 interaction with an explicit rule (place
alongside §2.20 and cross-reference §17.4 and §18.5):

> **Device driver logic lives in `drivers/<class>/<leaf>/`.** A `lib/*`
> device-support crate is permitted **only** when a non-driver consumer that is
> itself charter-legal shares it — a `kernel/arch/<target>/` bootstrap-floor
> path justified per §18.6, or a driver of a *different* class. "A transitional
> in-kernel scaffold also links it" is **not** a valid second consumer: that
> scaffold is the §18.5 defect to be removed, not a justification for hoisting
> device logic into `lib/*`. A `lib/*` device-support crate that loses its last
> charter-legal non-driver consumer is collapsed into its single `drivers/*`
> consumer (§2.2, §2.14).

- Record the rationale as a one-line entry in `PLAN.md` "Charter Amendments".
- Update §3 / §16.4 crate descriptions and any `docs/src/` pages the move
  touches (§2.8, §13).

## 3. Refactor (back into `drivers/`)

Apply the §1 test to each crate:

- **`lib/vl805`** — delete the in-kernel keyboard/USB bring-up scaffold (the
  §18.5 defect). Make the keyboard a **discovered user-space driver** (§18.3)
  matched from the driver store. Then **fold `lib/vl805` into
  `drivers/bus/usb/vl805/`** (its only remaining consumer), including the
  VL805-specific firmware-reload policy and the `build_xhci_node` /
  `reload_firmware_and_publish` wiring. Delete the obsolete crate, remove it
  from `Cargo.toml`, §3, §16.4, and `PLAN.md` (§2.14).
- **`lib/pcie_brcm`** — applied the same test. Decided from the real consumer
  graph: PCIe root-complex bring-up is **not** a floor item (the §18.6 floor is
  storage-only — virtio-blk + EMMC2 on aarch64, neither on PCIe), so its only
  honest consumer was `drivers/bus/pcie_brcm`. Folded in (its `lib` target).
- **`lib/vcmailbox`** — **stays in `lib/*`** (legitimate two-consumer split:
  aarch64 framebuffer boot console + `drivers/display/rpi_hvs`). Update its
  README to state *why* it qualifies under the amended rule and the others do
  not.
- **`lib/usb` / `lib/hid` / `lib/virtio*`** — re-check each against the amended
  rule. A bus-agnostic protocol engine shared by a `drivers/*` driver and a
  charter-legal non-driver consumer may remain; one whose only consumers are
  `drivers/*` of the same class collapses into the driver (or, if shared by two
  *different-class* drivers, stays per §2.2).

## 4. Structural option (decide with the User — §15.7)

If the goal is that **every** driver — including the in-kernel bootstrap floor
— lives physically under `drivers/`, the only clean route is to amend **§17.4**
to let `kernel/core` (the single selection point) depend on driver crates
explicitly marked `kind = "in-kernel"` (§8) floor drivers. Then a floor device
gets one home `drivers/<class>/<leaf>/` holding both its logic and its
`register` entry, and nothing device-specific need live in `lib/*`.

This is a larger charter change with real blast radius (the §17.4 one-way
`kernel/* ↛ drivers/*` invariant and the §17.5 `deps-check` enforcement). It
must be **decided deliberately by the User** before being treated as in scope;
do not assume it (§2.19).

## 5. Validation (definition of done — §7, §15.6, §23)

- Whole-project gate, foreground, never `-p`-scoped: `cargo fmt --all` (+
  `--check`), full `cargo xtask ci`, `cargo xtask fuzz --secs 5`, and
  `tools/ci/soak.sh both --secs 20`. Quote actual output (§23.4).
- `cargo xtask deps-check` / `cfg-check` still green: no new
  `kernel/* → drivers/*` edge (unless §4 is chosen and the invariant is
  formally amended), no board name or `cfg(target_arch …)` in a generic
  `lib/*` / arch-neutral `kernel/*` / framework / `userland/*` (§2.20, §17.2).
- The keyboard works as a **discovered** user-space driver (§18.3) with no
  in-kernel scaffold; QEMU/on-metal acceptance updated in `plans/PI.md` (P10).
- Every fixed defect carries a regression test (§7, §23.4). Docs/READMEs for
  every moved or deleted crate are updated in the same change (§2.8, §2.14).

## 6. Status

**done** — the minimal fix (§1 default; the User confirmed path A, not the §4
structural option). The in-kernel keyboard scaffold was already gone (the D5d
flip), so `lib/vl805` and `lib/pcie_brcm` each had only their sibling
`drivers/bus/...` crate as a consumer and were folded into them as
host-testable `lib` targets and deleted; `lib/vcmailbox` stays. §2.22 was added
to `AGENTS.md` (with the §2.20 / §17.4 / §3 / §16.4 cross-references) and logged
in `PLAN.md` "Charter Amendments"; `lib/hid` / `lib/usb` / `lib/virtio*` were
re-checked and remain legitimate shared protocol crates (no single-device
logic). Workspace, `tools/xtask`, and all `docs/src` pages were updated.
