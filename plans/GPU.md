# GPU.md — `lib/gpu`: the device seam, its backends, and real shader support

Binding under `AGENTS.md`. This plan owns how TAIRiX reaches a graphics
processor for **render and compute** work: the device-neutral seam its
consumers write against, the memory/submission/synchronisation model beneath
it, the per-device backends, and presentation. The shader toolchain — the IR,
its validator, the build-time builder, and the untrusted-input compiler — is
large enough and different enough in character to be its own plan
(`plans/SHADER.md`); this plan consumes it and does not restate it.

Scanout is `plans/DISPLAY.md`'s. Layer composition is
`plans/FIX-DISPLAY-ACCELERATION.md`'s, and **this plan is layered on it, not
beside it**.

Read first (§15.18): `plans/SHADER.md` (the IR and the compiler), `plans/FIX-DISPLAY-ACCELERATION.md`
(the `AcceleratedDisplay`/`AccelLayer` seam and the empty `gpu_virtio`
placeholder), `plans/DISPLAY.md`, `plans/FIX-DESKTOP-SPEEDUP.md` (the software
path that stays mandatory), `plans/PI.md` (VideoCore VI), `plans/WINTERSUN.md`
(the first demanding consumer), `AGENTS.md` §1 (Rust only), §4 (no ambient
authority, OOM as a value), §5.2 (capability minimalism), §16.4 (the curated
shared-library set), §17.3 (the software path is mandatory), §19.5 (parser
sandboxing), §24 and §26 (scalability and the operating-conditions floor), §27
(complete primitives).

## Ledger

| # | Item | Status |
|---|---|---|
| GP0 | This plan, `plans/SHADER.md`, the jump-sheet rows, the §3 map entries, the `PLAN.md` section | done |
| GP1 | `drivers/display/gpu_virtio`: the 2D driver — `register`, `BIND_KEYS`, resources, scanout, and the flip-completion interrupt | planned |
| GP2 | The memory and submission model: context-local GPU address spaces, typed allocations, command submission with validation, timeline fences, the reset watchdog, and the IOMMU requirement | planned |
| GP3 | `lib/gpu`: the device-neutral **render and compute** vocabulary, and the software backend — including the tile-wide SPIR-V interpreter that makes it complete rather than render-only | planned |
| GP4 | The pinned first-party pipeline set: OS chrome and the game's passes as build-time SPIR-V, with the conformance suite | planned |
| GP5 | The Venus backend: a guest Vulkan ICD over virtio-gpu's Venus capset — arbitrary shaders on real host hardware | planned |
| GP6 | Presentation: the swapchain, the accelerated layer path, fullscreen promotion, and the vsync flip | planned |
| GP7 | `plans/WINTERSUN.md` WS19: the game's terrain, entity, particle and light passes offloaded | planned |
| GP8 | Untrusted-shader admission: `plans/SHADER.md`'s runtime path wired into pipeline creation | planned |
| GP9 | `drivers/display/v3d`: the Raspberry Pi VideoCore VI backend | planned |
| GP10 | The application-facing GPU library and its generated C surface — what a ported game links | planned |

Items are built in ledger order. An item is complete — tests, docs, and a green
whole-project gate — before the next begins. Several items additionally wait on
`plans/SHADER.md`; that plan's ledger carries the mapping in its own `Blocks`
column and is the single place it is recorded.

### Milestones

Each exit criterion is a measurable artefact, not a checklist. Work does not
proceed past a milestone whose criterion is unmet.

| Milestone | Items | Exit criterion |
|---|---|---|
| **MG0 — pixels through hardware** | GP1 | `devmgr` autoloads `gpu_virtio` by discovery-match on all three bare-metal QEMU targets; a scanout resource is created, transferred and flipped; flip completion arrives as an interrupt and its waiter parks. |
| **MG1 — one API, software-complete** | GP2, GP3, GP4 | Every seam method has a software implementation; the compositor draws its chrome through pinned SPIR-V pipelines on the software backend; a consumer compiled against the seam runs unchanged with no device present. |
| **MG2 — real shaders on real hardware** | GP5, GP6, GP7 | The same pinned pipelines execute on the host GPU through Venus and produce the software backend's picture within tolerance; WinterSun's frame is drawn by the GPU and the gain is measured. |
| **MG3 — untrusted shaders** | GP8 | A shader module supplied at run time by an unprivileged process is compiled in a sandbox, admitted, executed, and cannot read another context's memory; a malicious module is refused or contained, and the fuzz corpus is clean. |
| **MG4 — native silicon** | GP9 | The Pi's V3D executes the pinned set from TAIRiX-generated ISA, conformant against the software reference. |
| **MG5 — a ported game runs** | GP10 | A third-party application, built against the published surface and linking no TAIRiX source, renders and presents. |

MG2 is the milestone that proves or kills the strategy: it is the first point at
which arbitrary shader code runs on a real GPU, and it is reachable **without
writing a single instruction-set back end**, because Venus forwards SPIR-V to
the host driver. That is why it precedes MG4 rather than following it.

## 0. Binding decisions

These are settled. A change that contradicts one stops and asks (§15.7).

1. **Shaders are a first-class, supported feature, not a refused one.** The
   goal is a GPU stack good enough to carry a modern game: programmable render
   and compute pipelines, arbitrary shader modules, and the performance that
   makes them worth having. A design that can only run a fixed set of effects
   is not that, and is refused.
2. **One API and one IR; the only variable is *when* a shader was compiled.**
   `lib/gpu` is the single device-neutral seam and **SPIR-V** is the single
   shader IR every backend ingests. A pipeline whose module was built at TAIRiX
   build time and one whose module arrived from an application at run time are
   the same object taking the same path; they differ only in who compiled the
   module and therefore in how much it must be distrusted. There is no second
   API for first-party code and no privileged path.
3. **The object model is Vulkan's, deliberately, and conformance is not
   claimed.** Devices, queues, typed allocations, descriptor/bind sets,
   pipelines, render passes, command buffers, timeline semaphores and fences:
   this is where explicit GPU APIs converged, it is what maps cleanly onto every
   real driver, and it is what makes the Venus backend a serialisation rather
   than a translation. Borrowing the model is not implementing the
   specification — TAIRiX does not ship `libvulkan` conformance until GP10 says
   what it does ship, and never claims partial conformance (§2.19).
4. **A shader compiler for a hardware backend is not a CPU JIT, and is not
   treated as one.** Its output is device code fetched by the GPU's shader cores
   from GPU-visible memory; nothing is mapped executable in a CPU address space,
   so §19.2's W^X transition and `CAP_JIT_MAP_EXEC` do not apply and are not
   required. The real attack surface is the **compiler itself** and the
   **submitted command stream**, and both are structurally contained (§0a).
   - **The software backend is the one place this could stop being true**, since
     its "device" is the CPU. It therefore **interprets** SPIR-V rather than
     generating machine code: an interpreter keeps the property that no
     untrusted module ever becomes CPU-executable, on every target including
     `wasm32`. Its speed comes from the width it interprets at — whole tiles of
     invocations at once, through `lib/cpuops`-dispatched SIMD kernels — not
     from code generation. Should a measurement ever show interpretation is
     insufficient, generating CPU code is a *new* decision requiring §19.2's
     full discipline and a capability gate, and is taken explicitly or not at
     all (§2.19).
5. **The software backend is mandatory, complete, and the reference.** Every
   seam method has a software implementation on every Tier-1 target including
   `wasm32`. A consumer writes one path; the absence of hardware is a
   performance property, never a capability gap to branch on (§17.3). The
   software backend is also the **conformance oracle**: a hardware backend is
   correct when it agrees with it within a stated tolerance, which is a test
   result rather than a hope.
6. **The OS's own pixels never depend on a runtime compiler.** The compositor's
   chrome, the desktop's effects and the game's passes are pinned, build-time
   SPIR-V (GP4). A broken, absent or refused shader compiler degrades
   third-party content; it can never fail to draw the desktop.
7. **No ambient access to the device.** A GPU context is created through a
   capability-gated request, holds a context-local address space, and can reach
   no memory it was not given (§2). A compromised client reaches its own
   resources and nothing else. Every submission is validated before it touches
   the ring (§5.4).
8. **Where an IOMMU exists, the GPU is behind it.** A device with unrestricted
   DMA to physical memory defeats every isolation property above, and a
   user-space driver holding such a device is a full-memory compromise waiting
   for one bug. TAIRiX requires IOMMU/SMMU containment where the platform
   provides it, and where it genuinely does not (some SoCs), that is recorded as
   a stated, per-platform security limitation in the driver's `README.md` — not
   discovered later.
9. **Nothing here is a second display path.** Rendering produces a surface;
   presenting it goes through the one existing display path and the compositor
   (§17.3, §2.2). A game does not seize the framebuffer, and neither does the
   compositor get a private channel to the device.

## 0a. The security position, stated honestly

Graphics stacks are a CVE farm for three reasons, and TAIRiX can structurally
remove all three. This is the strongest argument for doing this work here rather
than adopting someone else's stack, and it is the claim the plan is judged on.

| Why it goes wrong elsewhere | What TAIRiX does |
|---|---|
| The shader compiler runs in the application's (or the kernel's) address space with its full authority | It runs in a §19.5 minimum-capability sandbox: one shared-memory endpoint, no filesystem, no network, no spawn. A compiler bug yields control of a process that can do nothing. (`plans/SHADER.md`) |
| Compiler output is trusted because the compiler produced it | Output is **data**, validated on the way in — module structure, resource references, and bounds — and re-validated at submission. A backend never executes a module it did not itself validate. |
| A shader can address memory outside its context | Contexts have their own GPU address spaces; a submission naming a resource the context does not own is refused, not clamped (§5.4). |
| A hung or runaway shader wedges the display | A per-submission deadline, a reset path that recovers the device, and a contained client — the compositor survives a client's hang and reports it (§2.24, §26.5). |
| The device DMAs anywhere | Behind the IOMMU where one exists (decision 8). |

The residual risks are named rather than hidden: a bug in the **validator** (it
is the trust boundary, so it is fuzzed and property-tested as the first-class
security surface it is), a hardware erratum a driver must work around, and a
platform with no IOMMU.

## 0b. Why not simply adopt OpenGL

Because it costs more and buys less, for reasons that survived the challenge
that "Android and Java manage with a GL interface, so the C argument is weak".

**That challenge is correct and the C argument is withdrawn.** `GLES31` is a
Java class over JNI, WebGL is a JavaScript binding, `wgpu` is Rust: the language
a specification is written in says nothing about the language of a binding or an
implementation, and §1/§15.11 forbid *authoring* C, not implementing a spec that
was specified in C. That reason is deleted rather than softened.

What remains, and is sufficient:

1. **No GPU is tied to OpenGL.** None has been since roughly the GeForce 3 era.
   Hardware is tied to its command stream, its register layout, its shader ISA
   and its memory model; GL is itself a thick translation layer over those, and
   every implementation translates. Adopting GL therefore means writing the
   per-chip translation that is needed **anyway**, plus a thirty-year
   compatibility state machine on top of it. It is strictly more work, not less.
2. **The state machine is the part that aged worst**, and is exactly what the
   explicit model in decision 3 exists to avoid: implicit synchronisation,
   driver-guessed hazard tracking, and hundreds of setters whose interactions
   are the bug surface.
3. **Its value is compatibility with software TAIRiX does not yet run**, and
   when TAIRiX does want to run it (GP10, MG5), the API that software targets is
   Vulkan or a translation onto Vulkan — which is the model already chosen.
4. **Partial conformance is a misleading claim** (§2.19). "OpenGL" means the
   conformance suite, and a subset shipped under the name misleads every
   consumer.

**The cost of not adopting a standard is real and is accepted with its eyes
open**: no external conformance suite to run, no corpus of open implementations
written in TAIRiX's vocabulary, and no vendor documentation phrased in its
terms. Three things mitigate it and are why the trade is taken. The object model
*is* the industry's (decision 3), so vendor documentation and open drivers
remain readable. The IR *is* Khronos SPIR-V (`plans/SHADER.md`), so the largest
single piece of specification work is adopted rather than invented. And the
software backend is an **exact** differential oracle (decision 5), which for
TAIRiX's purposes is a stronger check than a per-test-tolerance conformance
suite.

## 1. GP3 — what `lib/gpu` is

An explicit render **and compute** vocabulary at the altitude modern APIs
settled on: resources created up front, state gathered into immutable objects,
work recorded into command buffers and submitted with explicit synchronisation.

- `Device` — an adapter opened through the driver seam, reporting an honest
  capability set. There is always at least one: the software backend.
- `Allocation`, `Buffer`, `Image` — typed, sized, usage-declared, with explicit
  transfer and no implicit copies. Memory types are enumerated, not assumed.
- `BindSetLayout`, `BindSet` — a validated group of resources bound in one
  checked operation rather than slot-by-slot mutation.
- `ShaderModule` — a **validated** SPIR-V module (`plans/SHADER.md`). The seam
  ingests only validated modules; validation is not the backend's business and
  never happens twice.
- `RenderPipeline`, `ComputePipeline` — a module plus its fixed-function state,
  immutable once created. Compute is present from the start, not added later:
  it is what makes the GPU useful for more than drawing, and a pipeline object
  with no compute variant is the incomplete primitive §27 forbids.
- `Pass` — a render-target set with a load/store action per attachment and a
  scissor, so a damage-scoped frame costs a damage-scoped pass.
- `CommandBuffer`, `Queue`, `TimelineSemaphore`, `Fence` — record, submit, and
  wait. Waiting parks on the driver's completion interrupt, never a busy-poll
  (§2.23).
- `Swapchain` — presentation, resolved against the compositor (GP6).

## 2. GP2 — memory, submission, isolation

This is the layer the previous draft of this plan omitted entirely, and it is
where the security properties actually live.

- **A context is the unit of isolation.** Creating one yields a GPU address
  space private to it. Resources are bound into that space and nowhere else; two
  contexts sharing a surface do so through an explicit, capability-carried
  export, never by naming each other's handles.
- **Submission is validated, then queued.** Every command buffer is checked
  before it reaches the ring: every referenced resource belongs to the
  submitting context, every offset and extent is in bounds, no privileged
  register is written, and every pipeline is one this context created. A
  submission failing any check is refused whole as a typed error — never
  partially applied, never clamped into range (§5.4, §23.1).
- **Synchronisation is explicit and timeline-based.** Timeline semaphores order
  work within and across queues; fences report completion to the CPU. A waiter
  parks on the device interrupt.
- **Deadlines and reset.** Every submission carries a deadline. A submission
  that overruns it marks its context lost, resets the device through the
  driver's reset path, and reports the loss to the client as a typed error; the
  compositor and every other context survive. A client whose context is lost
  recreates it — the device is never left wedged and nothing retries forever
  (§2.1, §26.5).
- **Capacities scale, bounds stay fixed** (§24). Allocation counts, queue depth
  and the resident-resource budget derive from the discovered device and RAM and
  grow on demand; the *validation* bounds — maximum module size, maximum
  binding count, maximum submission size — are security bounds and are fixed
  (§24.4).
- **Memory accounting is per context and bounded.** A client cannot exhaust
  device memory for every other client; a request beyond its effective limit
  fails closed as a typed error after growth is attempted (§24.3, §26.2).

## 3. GP1 / GP5 / GP9 — the backends

**GP1 has the clearest independent value and lands first.**
`drivers/display/gpu_virtio` is today a two-line `#![no_std]` placeholder, so
the desktop's accelerated display path is unreachable on every QEMU target. It
lands as a user-space driver on `lib/virtio` + `lib/drvrt` + `lib/dma-barrier`,
bound by discovery-match (§18.3), holding only the register window, DMA
constraint and interrupt its matched node requested. It serves
`plans/FIX-DISPLAY-ACCELERATION.md` whether or not anything above it follows.

**GP5 is the strategic one: Venus.** virtio-gpu's Venus capset carries
serialised Vulkan commands with SPIR-V modules to the host, which replays them
on the host's real driver. The guest side is therefore a Vulkan ICD — large but
**mechanical**, with no shader compiler and no instruction-set back end. That is
what makes "arbitrary shaders on real hardware" reachable on QEMU without years
of per-chip work, and it is why decision 3 chose Vulkan's object model: the
backend is a serialisation of the seam, not a translation of it.

The alternatives are named and refused for this role. **virgl** carries a
Gallium/TGSI-flavoured stream and would make the guest speak a GL-shaped
dialect — the compatibility tail of §0b with none of its compatibility. **drm
native context** forwards a real driver's uAPI, which means implementing that
driver's uAPI anyway.

**Where the device advertises only 2D**, which is the common QEMU
configuration, the device still accelerates transfer, scanout and flips and
rendering stays software. That is a first-class outcome with its own tests, not
a degraded one.

**GP9 is the first real instruction-set back end.** The Pi's VideoCore VI is
documented and has an open driver to read; a SPIR-V→V3D compiler and command
builder is the genuinely large piece of work in this plan, and it is sequenced
last because Venus proves everything above it first. Its value is measured at
GP5, not assumed.

## 4. GP6 — presentation

A rendered surface is presented through the **one** existing display path: the
compositor's accelerated layer seam
(`plans/FIX-DISPLAY-ACCELERATION.md`), including the scanout-sized fullscreen
promotion `plans/WINTERSUN.md` P3 needs. The swapchain is the seam's client-side
view of that: acquire, render, present, with the flip's completion arriving as
the driver's interrupt.

The division is worth stating because the two are easy to conflate: **the layer
path places and blends finished pictures; `lib/gpu` computes the pictures.** A
2D game's cost is not in placing images — terrain is a per-pixel weighted
material blend, lighting is an accumulation pass, weather is thousands of
sprites — none of which a stack of hardware planes expresses. Using the layer
path for that would mean composing a scene from hundreds of planes no hardware
has.

## 5. GP4 / GP8 — the two compile times

The same pipeline object, reached two ways.

**Build time (GP4) — the pinned set.** The OS's chrome and the game's passes
are first-party SPIR-V modules **built in Rust** by `plans/SHADER.md`'s builder,
emitted by an xtask, committed to the tree, and verified on drift — the pattern
`cargo xtask c-header --write` and `cargo xtask font-atlas` already establish.
The consequences are the point: the desktop carries no runtime compiler surface
at all, the modules are reviewable artefacts with pinned conformance cases, and
a change to a shader either produces identical output or fails the gate.
Authoring them in Rust rather than a shading language is also what keeps §1
intact — TAIRiX writes no third language.

**Run time (GP8) — untrusted admission.** A module supplied by an application
is decoded and validated in `plans/SHADER.md`'s sandbox, and only a validated
module becomes a `ShaderModule`. This is the path a third-party game uses and
the one the threat model in §0a is written against.

A consumer cannot tell the two apart at the seam, which is the property that
keeps this one API rather than two.

## 6. Layering and what stays true

- `lib/gpu` depends only on `lib/*` (§17.4) and reaches a device through the
  existing display/driver IPC seam.
- It carries no board or SoC name (§2.20). Device specifics live in the driver
  leaf, which may know its hardware because that is its job; V3D's quirks belong
  in the V3D driver and nowhere above it.
- Drivers stay user-space and capability-bound (§4, §18.3).
- The software backend is never removed, on any target.
- The headless build (§17.3) is unaffected: nothing here is required to boot,
  log in, or run a text session.

## 7. Refused by name

- **OpenGL, OpenGL ES, or a GL-shaped compatibility layer**, for §0b.
- **Implementing Vulkan or WebGPU *conformantly* as a specification**, or
  claiming partial conformance under either name (§2.19). The object model is
  borrowed; the conformance claim is not made.
- **An external graphics crate, or Mesa in any form** (§1, §2.12).
- **A GPU-only code path with no software equivalent**, and any consumer branch
  of the form "if accelerated, draw it this way" (§17.3, §2.2).
- **A private game or compositor back-channel to the device** (§17.3).
- **A second rasteriser or blend path.** The software backend shares
  `lib/raster`'s scan converter (§2.2).
- **Executing a shader module the validator did not admit**, in any mode,
  including a "trusted first-party" bypass. The pinned set is validated too — it
  is cheap and it keeps one path.
- **Clamping an out-of-range submission into range** instead of refusing it.
- **A hand-picked constant as a device capacity** (§24.1), or widening a
  validation bound to admit an oversize module (§24.4).

## 8. Charter amendments this work requires

Each needs sign-off before the item that depends on it; none is assumed.

1. **§16.4 — a new curated shared-library class.** If third-party applications
   reach the GPU (GP10), they must link an OS-provided GPU library
   *dynamically*, so one security update covers every consumer. §16.4's set is
   closed and lists no graphics-device class. Until GP10 the seam is statically
   linked by first-party consumers and no amendment is needed; GP10 cannot land
   without one. **Blocks GP10.**
2. **A new capability gating GPU context creation.** Against §5.2's three
   tests: it guards a class of resources rather than one object (all
   device-memory allocation, submission, and DMA-capable mapping); it arrives
   with its enforcement point and a live holder rather than ahead of them; and
   no existing capability expresses it — `CAP_SHM` grants shared memory, not
   device submission, and `CAP_DRV_LOAD` is the driver's, not the client's. I
   judge it passes, but adding to the capability vocabulary is your call.
   **Blocks GP2.**
3. **§3 map entries** for `lib/gpu`'s changed description and for
   `plans/SHADER.md`'s crates. Prose within a section; no gate implication.

## 9. Verification

- **GP1**: binds by discovery-match under `devmgr` on aarch64 `virt`, riscv64
  `virt` and x86_64 q35; resource create/transfer/flush/flip; flip completion
  parks on the interrupt; the desktop's accelerated seam drives it end to end.
- **GP2**: a submission naming another context's resource is refused; an
  out-of-bounds offset is refused whole, not clamped; a context cannot map
  memory it was not granted; an overrunning submission loses its context and
  resets the device while every other context survives; per-context memory
  limits fail closed after growth is attempted.
- **GP3**: every seam method has a software implementation and a host test; a
  program compiled against the seam runs unchanged with no device present.
- **GP4**: the pinned modules are regenerated reproducibly, verify-mode fails on
  drift and runs in `ci`, and every pipeline's conformance case pins its
  software output.
- **GP5**: the pinned set executes through Venus and matches the software
  reference within its stated tolerance; a 2D-only device degrades to software
  rendering with accelerated transfer and flip, and *that* path is tested, not
  assumed.
- **GP8**: a hostile module is refused or contained; a compromised compiler
  sandbox can reach nothing; the fuzz corpus is clean (`plans/SHADER.md`).
- **Differential**: a frame through the seam matches the direct software path
  within tolerance, so the seam itself is proven not to change the picture.
- **`miri`**: `lib/gpu` and every driver here are enrolled from their first
  item — all will carry `unsafe` for DMA and MMIO.
- **`loom`**: mandatory for the submission/completion ring and the fence/
  timeline handoff, both lock-free producer/consumer protocols whose correctness
  is an ordering claim (§19.11).
- **Performance is measured, not claimed** (§2.16): a stated scene at a stated
  resolution, software versus accelerated, recorded. A quoted measurement says
  whether the scene contained the frosted chrome that
  `plans/FIX-DISPLAY-ACCELERATION.md` explains still forces a software
  composite.
- **The floor** (§26.7): the resident GPU-resource budget is bounded and derived
  from discovered memory; a small machine with a large working set degrades in
  performance, never in correctness, and never panics.

## 10. Open decisions

1. **GP9's value.** A first-party V3D back end is large, and how much of the
   desktop and the game are GPU-bound on that board is a measurement from GP5,
   not a preference.
2. **What GP10 actually publishes.** A Vulkan-shaped C surface generated from
   Rust definitions (§9) is the obvious answer and is enormous; a smaller
   TAIRiX-native surface is tractable but is a porting burden on every game. The
   decision needs MG2's measurements and a stated target for what "a modern
   game" means concretely, and is deliberately left open rather than guessed.
3. **Whether compute is exposed to applications at GP10 or held to first-party
   use.** Compute is in the seam from GP3 because the renderer needs it;
   publishing it to third parties widens the attack surface and is a separate
   decision.
