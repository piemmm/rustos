# GPU.md — `lib/gpu`: the device-neutral render seam, and why it is not OpenGL

Binding under `AGENTS.md`. This plan owns how TAIRiX reaches a graphics
processor for *rendering work* — not scanout, which `plans/DISPLAY.md` owns,
and not layer composition, which `plans/FIX-DISPLAY-ACCELERATION.md` owns. It
exists because `plans/WINTERSUN.md` asked for OpenGL, the honest answer is no,
and the honest answer needs a plan behind it rather than a refusal.

Read first (§15.18): `plans/FIX-DISPLAY-ACCELERATION.md` (the
`AcceleratedDisplay`/`AccelLayer` seam, the empty `gpu_virtio` placeholder, and
the flip/vsync work — **this plan is layered on it, not beside it**),
`plans/DISPLAY.md`, `plans/FIX-DESKTOP-SPEEDUP.md` (the software path that stays
mandatory), `plans/PI.md` (VideoCore VI), `AGENTS.md` §1 (Rust only), §9 (the C
surface is generated, never authored), §16.4 (the curated shared-library set),
§17.3 (the software path is the mandatory fallback), §19.2 (W^X and
`CAP_JIT_MAP_EXEC`), §2.12 (roll your own).

## Ledger

| # | Item | Status |
|---|---|---|
| GP0 | This plan, the jump-sheet row, the `PLAN.md` section | done |
| GP1 | `drivers/display/gpu_virtio`: a real user-space virtio-gpu driver — `register`, `BIND_KEYS`, 2D resources, scanout, and the flip-completion interrupt | planned |
| GP2 | `lib/gpu`: the device-neutral render vocabulary — devices, buffers, images, bind sets, passes, command lists, fences — and the **software backend** that implements all of it | planned |
| GP3 | The material-kernel registry: the closed set of named render kernels, each with a software implementation, and the conformance suite that pins their output | planned |
| GP4 | The virtio-gpu backend of `lib/gpu` behind the device's advertised 3D capability, with the conformance suite passing on both backends | planned |
| GP5 | `plans/WINTERSUN.md` WS19: the game's terrain, entity, particle, and light passes expressed as kernels and offloaded | planned |
| GP6 | A Raspberry Pi VideoCore VI (V3D) backend | blocked: needs the open decision below |

`lib/gpu` does **not** exist until GP2, and GP2 does not land before GP1. A
render API with no backend is speculative interface (§2.3, §2.4); a backend
with no device to drive it is worse. The order is deliberate.

## 0. Why not OpenGL

The request was "OpenGL compatibility (or whatever succeeds it that you can
reliably build)". The second half is the answerable one, for five reasons that
are structural rather than a matter of preference:

1. **OpenGL is a C API, and TAIRiX does not author C.** §1 and §15.11 forbid
   writing C here; §9 permits a C-visible surface only as *generated* output of
   `lib/abi`, emitted by `cargo xtask c-header`. A GL implementation is a large,
   hand-authored C surface — hundreds of entry points, `GLenum` constants, and
   headers — that is not generated from anything and could not be.
2. **Its value is compatibility with software TAIRiX does not have.** GL earns
   its keep by running existing GL programs. There are none here, and the
   beneficiary would be a hypothetical third-party port — a different and far
   larger project than making TAIRiX's own desktop and game fast.
3. **It needs a shader compiler, which is an untrusted-code surface.** GLSL in,
   machine code out, at runtime, requiring `CAP_JIT_MAP_EXEC` and the
   `RW`→`RX` transition of §19.2. That is the single largest attack surface in
   a modern graphics stack and the source of a long CVE history, taken on for a
   compatibility benefit item 2 shows is currently zero.
4. **There is nothing to run it on.** `drivers/display/gpu_virtio` is an empty
   placeholder — `#![no_std]` and nothing else. A GL front end over no driver
   accelerates nothing; GP1 is the work that actually matters, and it is
   needed whichever API sits above it.
5. **Conformance is the real cost.** A GL implementation is judged against a
   conformance suite, and a partial one is a compatibility claim that misleads
   every consumer. Claiming "OpenGL" and shipping a subset is the kind of
   almost-true that §2.19 exists to forbid.

So: build the seam a modern GPU is actually reached through, keep the software
renderer mandatory and complete, and make the accelerated path a measured
improvement rather than a requirement.

## 0a. The other obvious challenge: it is a 2D game, so why a render API?

The desktop already has an accelerated *layer* path
(`plans/FIX-DISPLAY-ACCELERATION.md`): hardware planes, position, scale, alpha,
damage, flips. A 2D top-down game looks like exactly the workload that path was
built for, and a reviewer should ask why it is not simply used.

Because layer composition can place and blend finished pictures, and the
game's cost is not in placing pictures — it is in *computing* them per pixel:

- **Terrain is a per-pixel weighted material blend** with a height-offset
  comparison across several materials plus detail noise (`plans/WINTERSUN.md`
  §3). That is not "draw this bitmap here"; the bitmap does not exist until the
  blend has run. No stack of layers expresses it, because the operation is a
  function of several sources at each pixel, not an ordering of them.
- **Lighting and fog are accumulation passes**, and a low sun means slope-shaded
  terrain with directional shadows — again a per-pixel computation over inputs,
  not a composite of prepared images.
- **Weather and particles** are thousands of small alpha-blended sprites per
  frame, which is a throughput problem a plane count in the low tens cannot
  address.

So the honest division: the layer path is the right mechanism for **presenting**
the game's finished frame (and for the compositor's own job, including the
fullscreen promotion `plans/WINTERSUN.md` P3 needs), and it is used for that.
`lib/gpu` exists for the per-pixel work that produces the frame in the first
place. Using the layer path for the latter would mean composing a scene from
hundreds of planes no hardware has — and the game would still have to blend the
terrain itself, in software, which is the work it was trying to offload.

The corollary is worth stating because it bounds this plan's value: the game is
**fully playable with `lib/gpu` absent**, since the software backend is the
reference implementation (§1). This is an optimisation with a measured payoff
(§6), not a dependency.

## 1. What `lib/gpu` is

An **explicit** render vocabulary at the altitude modern APIs settled on —
resources created up front, state gathered into immutable objects, work
recorded into command lists and submitted with explicit synchronisation. Not a
state machine with two hundred setters, which is the part of GL that aged worst.

- `Device` — an adapter opened through the display/driver seam, reporting an
  honest capability set. There is always at least one: the software backend.
- `Buffer`, `Image` — typed, sized, usage-declared allocations with explicit
  upload and no implicit copies.
- `BindSet` — a validated group of resources bound together, so binding is one
  checked operation rather than a slot-by-slot mutation.
- `Pass` — a render target set, a load/store action per attachment, and a draw
  list. Damage-aware: a pass states its scissor, so a partial frame costs a
  partial pass.
- `Kernel` — a **named** material/render kernel from the closed registry (§2),
  parameterised by a bind set. This is where a shader would be, and is not one.
- `CommandList`, `Fence` — record, submit, and wait. Waiting parks on the
  driver's completion interrupt (never a busy-poll, §2.23).

**Every method is implemented by the software backend.** That is what makes the
seam honest: a consumer writes one path, and the absence of hardware is a
performance property, not a capability gap it has to branch on (§17.3). The
software backend is not a stub — it is the same tiled, threaded, `lib/cpuops`-
dispatched raster path the game already uses, expressed behind the seam, and it
shares `lib/raster`'s one scan converter and blend (§2.2).

## 2. GP3 — named kernels instead of a shader language

The decision that makes the rest tractable: **there is no runtime shader
compiler, and no shading language.** The render kernels are a closed,
versioned registry of first-party operations — terrain material splat,
height-weighted material blend, sprite composite, tinted mask blit, particle
accumulate, light accumulate, fog and atmosphere composite, blur, colour
grade, and the handful more the desktop and the game actually need.

Each kernel has:

- a documented signature (its bind-set shape and its parameters),
- a **software implementation** in Rust, which is the reference,
- a per-backend implementation where a backend can do better,
- and a **conformance case** pinning its output for fixed inputs.

The consequences are the point. No JIT and no `CAP_JIT_MAP_EXEC`. No
untrusted-input compiler. A kernel is reviewable Rust with tests. And because
the software implementation is the reference, the conformance suite can assert
that a hardware backend agrees with it within a stated tolerance — so "the
accelerated path draws the same picture" is a test result, not a hope.

The honest cost: a consumer cannot invent a new visual effect without adding a
kernel to the registry, with its implementations, its tests, and its docs. For
a first-party OS and a first-party game that is a review gate rather than an
obstacle; for third-party arbitrary shader authoring it would be prohibitive,
and that is a deliberate non-goal.

## 3. GP1/GP4 — the backends

**GP1 is the work with the clearest independent value.** `drivers/display/gpu_virtio`
is the QEMU acceleration path on aarch64 `virt`, riscv64 `virt`, and x86_64 q35
— all three expose virtio-gpu — and it does not exist, so `devmgr` can never
autoload it and the desktop's accelerated path is unreachable. It lands as a
user-space driver on `lib/virtio` + `lib/drvrt` + `lib/dma-barrier`, exactly
like the existing virtio and USB user-space drivers, bound by discovery-match
(§18.3), holding only the register window, DMA constraint, and interrupt its
matched node requested. It serves `plans/FIX-DISPLAY-ACCELERATION.md` whether
or not `lib/gpu` ever follows.

**GP4 layers the render backend on it**, gated on the device's advertised
capability. Where the host exposes only 2D, the device still accelerates
resource transfer, scanout and flips, and rendering stays software — which is
the common QEMU configuration and must therefore be a first-class outcome
rather than a degraded one.

**The compositor and the game share this seam.** A game does not get a private
GPU path: that would be the back-channel §17.3 forbids and the second present
path §2.2 forbids. A game renders into its own window surface through
`lib/gpu`; the compositor composites through the accelerated display seam;
both reach the same driver through the same discovery-bound capabilities.

## 4. Layering and what stays true

- `lib/gpu` is a `lib/*` crate depending only on `lib/*` (§17.4). It reaches a
  device through the existing display/driver IPC seam; it adds **no syscall**
  and no `lib/abi` surface beyond what that seam already carries.
- It is **statically linked** by its consumers, so it adds no class to §16.4's
  closed curated shared-library set and needs no charter amendment. Were it ever
  to become a dynamically linked OS library, that would require amending §16.4
  and `PLAN.md` — and it is not proposed here.
- It carries no board or SoC name (§2.20). Device specifics live in the driver
  leaf, which may know its hardware because that is its job; V3D's quirks
  belong in the V3D driver and nowhere above it.
- The software backend is mandatory and never removed, on every Tier-1 target
  including `wasm32`, where there are no registers to reach.

## 5. Refused by name

- **OpenGL, OpenGL ES, and a GL-shaped compatibility layer**, for §0's five
  reasons.
- **A shading language, a bytecode format, or any runtime shader compilation.**
  Kernels are first-party Rust (§2).
- **Vulkan or WebGPU as an *implemented specification*.** Their *altitude* is
  borrowed; their surface area is not. Implementing either conformantly is the
  §0 item-5 trap in a different font.
- **An external graphics crate, or Mesa in any form.** §1, §2.12.
- **A GPU-only code path with no software equivalent**, and any consumer branch
  of the form "if accelerated, draw it this way" (§17.3, §2.2).
- **A private game or compositor back-channel to the device** (§17.3).
- **`RWX` mappings anywhere in the stack** (§19.2).

## 6. Verification

- GP1: the driver binds by discovery-match under `devmgr` on all three
  bare-metal QEMU targets; a scanout resource is created, transferred, and
  flipped; flip completion arrives as an interrupt and the waiter parks rather
  than polls; the desktop's accelerated display seam drives it end to end.
- GP2: every seam method has a software implementation and a host test. A
  consumer program compiled against the seam runs unchanged with no device
  present.
- GP3: every kernel's conformance case pins its software output; a hardware
  backend's output agrees within its stated tolerance; a kernel added without
  its conformance case fails the gate.
- GP4: the conformance suite passes on both backends; a 2D-only device
  degrades to software rendering with accelerated transfer and flip, and that
  path is tested, not assumed.
- A frame rendered through the seam matches the frame rendered by the direct
  software path within tolerance, so the seam itself is proven not to change
  the picture.
- `miri` enrolment for `lib/gpu` and the driver, both of which will carry
  `unsafe` for DMA and MMIO; `loom` for the submission/completion ring, which
  is a lock-free producer/consumer protocol whose correctness is an ordering
  claim (§19.11).
- Performance is measured, not claimed: a stated scene at a stated resolution,
  software versus accelerated, with the measurement recorded. A quoted
  measurement must say whether the scene contained the frosted chrome that
  `plans/FIX-DISPLAY-ACCELERATION.md` explains still forces a software
  composite.

## 7. Open decisions

1. **GP6, the V3D backend.** Raspberry Pi 4's VideoCore VI is documented and
   has an open-source driver to read, but a first-party Rust V3D backend is a
   large piece of work whose value depends on how much of the desktop and the
   game are actually GPU-bound on that board. It is `blocked` on a measurement
   from GP4, not on a preference.
2. **Whether the game's particle and light accumulation belong in the kernel
   registry at all.** Both are bandwidth-bound rather than arithmetic-bound, so
   the software path may already be at the memory limit and offload may buy
   nothing. Decided by measurement at GP5 (§2.16).
3. **Compute-only use.** World generation is embarrassingly parallel and a GPU
   could run it, but the seam as specified is a *render* vocabulary. Extending
   it to general compute is a genuine addition to a closed surface and would
   need its own justification (§2.4), not an incidental widening.
