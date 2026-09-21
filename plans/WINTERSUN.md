# WINTERSUN.md — the desktop RPG: world, rules, realm server, and client

Binding under `AGENTS.md`. WinterSun is a 2D top-down isekai action-RPG with a
procedurally generated world, server-authoritative multiplayer, and a
self-balancing economy. This plan owns the game: what it simulates, what it
draws, how a client and a realm server speak, and where each piece lives.

These companion plans own the cross-cutting pieces the game is a consumer of,
because each has consumers beyond it and a single definition is the charter's
rule (§2.2, §6). The GPU and shader plans are no longer driven by this game —
they are an OS workstream in their own right, and the game is one demanding
consumer of them:

| Concern | Plan |
|---|---|
| The parametric figure, its rig, its clips, and the art-quality harness | `plans/FIGURE.md` |
| Durable, crash-safe, indexed record storage | `plans/RECDB.md` |
| The device-neutral GPU render and compute seam, and its backends | `plans/GPU.md` |
| Shader programs: the IR, the validator, and the sandboxed compiler | `plans/SHADER.md` |

Read first (§15.18): `AGENTS.md` §10 (asset tiers, DPI), §16.5 (bundles),
§17.3 (the optional-desktop edge), §19.5 (parser sandboxing), §24 and §26
(scalability and the operating-conditions floor), §27 (complete primitives),
§28 (interactive surfaces answer within a frame); `plans/SOUND.md` (the audio
stack this consumes — **not** re-derived here), `plans/GUI-CONTROLS-DESIGN.md`
(every control the UI composes), `plans/COMPOSITOR-WORK.md` (window furniture
and size states), `plans/DISPLAY.md` (the seat lease), `plans/APPWIN.md` (the
window channel), `plans/NETWORK.md` (the socket ABI), `plans/APPDATA.md` (per-app
settings), `plans/CINDER.md` (the in-tree procedural-creature precedent
`plans/FIGURE.md` generalises), `plans/ICONS.md` (the artwork pipeline),
`plans/APPS.md` (bundle, help, and command-app rules).

## Ledger

| # | Item | Status |
|---|---|---|
| WS0 | This plan, `plans/FIGURE.md`, `plans/RECDB.md`, `plans/GPU.md`, the jump-sheet rows, the §3 map entries, the `PLAN.md` stage | done |
| WS1 | `Layer::UserGame` in `deps-check`, the `userland/games/` subtree, and `wintersun/net`: the wire vocabulary, framing, the authenticated session handshake, bounded decode, the fuzz target | done |
| WS2 | `wintersun/world`: the seed-pure chunked generator — uplift, hydrology, climate, biomes, roads, sites — and its cross-architecture determinism vertical | done |
| WS3 | `wintersun/rules`: the fixed-tick authoritative step, space and collision, stats, damage, status effects | done |
| WS4 | `wintersun/art`: material synthesis, the splat field, the decal and particle vocabulary, the WinterSun palette | done |
| WS5 | The client shell: window, the three size states, input, frame pacing, camera, terrain draw | done |
| WS6 | Figures on screen: presets, clips, the animation state machine, the locomotion join | planned |
| WS7 | `Code/wintersun-store`: the schemas and the realm's single writer | planned |
| WS8 | `Code/wintersund` + `Code/wintersun-zone`: the gateway, zone shards, interest management, back-pressure, the thousand-player floor | planned |
| WS9 | Combat: melee, ranged ballistics, traps, the archetypes | planned |
| WS10 | Magic: casts, channels, spell shapes, the effect vocabulary, visual effects | planned |
| WS11 | Skills, levelling, items, equipment slots, the configurable action bar | planned |
| WS12 | The economy: the faucet/sink ledger and the bounded price controller | planned |
| WS13 | Weather and sky: fronts, precipitation, fog, lightning, wind, the day/night cycle | planned |
| WS14 | Audio: the game's voice bed over `audio-v1` | planned |
| WS15 | Chat, moderation, and the audit trail | planned |
| WS16 | The in-game console, `wintersunctl`, and the admin surface | planned |
| WS17 | The character designer | planned |
| WS18 | Accessibility, localisation, and the settings pane, including the detail-level control | planned |
| WS19 | GPU offload behind `lib/gpu` | planned |

Items are built in ledger order. An item is complete — tests, docs, and a green
whole-project gate — before the next begins.

### Milestones

The ledger is a build order; it is not a delivery plan, and a plan that only
becomes playable at item eight is one nobody can steer. These are the
milestones the ids group into, each with an **exit criterion that is a playable
or measurable artefact**, not a checklist. Work does not proceed past a
milestone whose exit criterion is unmet.

| Milestone | Items | Exit criterion |
|---|---|---|
| **M0 — the ground** *(met)* | WS1 | The `userland/games/` subtree exists, `deps-check` enforces `Layer::UserGame`, and the wire protocol round-trips and fuzzes clean. |
| **M1 — a world you can walk in** *(the vertical slice)* | WS2, WS3, WS4, WS5, WS6 | One character walks over generated terrain, in a window and in exclusive fullscreen, inside the §3 frame budget, with the state hash identical on all four Tier-1 targets. This is the milestone that proves or kills the software renderer. |
| **M2 — a world you share** | WS7, WS8 | Two clients on one realm see each other move, characters persist across a restart, a zone handover works, and an uncleanly disconnected client leaves the realm intact at the last committed state. |
| **M3 — a game** | WS9, WS10, WS11 | The core loop is playable end to end — fight, win, level, equip, spend — and the §5 game-feel budget is met at a simulated 100 ms round trip. |
| **M4 — a world worth being in** | WS12, WS13, WS14, WS15, WS16, WS17, WS18 | Economy stable over the shock set; weather, audio, chat, admin, designer and accessibility all live. |
| **M5 — acceleration** | WS19 | The accelerated path draws the same picture as the software path within tolerance, with the gain measured rather than claimed. |

M1 is deliberately the riskiest milestone and deliberately early: if a
first-party software renderer cannot hold the budget, everything downstream is
built on a wrong assumption, and the cheapest time to learn that is before the
netcode exists.

### Prerequisites owned by other plans

None of these is restated here; each is a hard dependency named so it cannot be
discovered late.

| # | What is needed | Owner | Blocks |
|---|---|---|---|
| P1 | The audio stack exists at all: the PCM vocabulary, `audio_ring`, `audio-v1`, `audiochan-v1`, the engine, one driver, `audiod` | `plans/SOUND.md` SND2–SND4 | WS14 |
| P2 | `lib/sound`'s decoder registry and the sandboxed decode seam | `plans/SOUND.md` SND9 | WS14 |
| P3 | Window **size states** — `Restored` / `Maximized` / `Fullscreen` — on the window channel, and the compositor promoting a scanout-sized fullscreen surface to a single layer | `plans/COMPOSITOR-WORK.md` Stage J | WS5 — **done** |
| P4 | `lib/crypto` gains X25519 key agreement (`lib/crypto::agree`, over `x25519-dalek` 2.0.1 — pinned to the 2.x line so it shares the `curve25519-dalek` 4.x and `rand_core` 0.6 already beneath `ed25519-dalek`; its `zeroize` feature also pulls the compile-time `zeroize_derive`, so the footprint is that crate plus one proc macro rather than the single crate first estimated) | `lib/crypto` | WS1 — **done** |
| P5 | Durable storage: `lib/recdb` through its transactional and recovery items | `plans/RECDB.md` RD1–RD6 | WS7 |
| P6 | The figure engine: shapes, rig, clips, blending, and the art harness | `plans/FIGURE.md` FG1–FG5 | WS6 |
| P7 | The GPU seam with a live backend | `plans/GPU.md` GP1–GP6 | WS19 |

P3 was the only prerequisite that changes a shipped desktop contract, and it
landed as `plans/COMPOSITOR-WORK.md` Stage J. What WS5 can now rely on:
`WindowSizeState` is three-valued and lives in `lib/abi` (re-exported by
`lib/controls`); `WindowRequest::SetSizeState` asks, and the applied state
comes back on `WindowEvent::Resized` **beside** the new client extent, so an
app never learns one without the other. Fullscreen takes the scan-out rather
than the work area, ignores the app's content ceiling, raises the window over
the taskbar, and withdraws the decoration without discarding it.

**Exclusive fullscreen is not a second display path.** A game does not seize
the framebuffer: it asks for `Fullscreen`, the compositor sizes its surface to
the scanout and promotes it to a single unblended layer, and the present goes
through the one existing display path. That is where exclusive fullscreen's
real benefit lives — no composition pass, a tear-free flip — and taking it any
other way would be the private back-channel §17.3 forbids and the second blend
path §2.2 forbids. The promotion is `Compositor::fullscreen_cover`, and it
waits for a frame that genuinely covers the scanout: until the client presents
at the new extent the scene composites normally, because a promoted layer has
nothing beneath it to show through.

## 0. Binding decisions

These are settled. A change that contradicts one stops and asks (§15.7).

1. **The server is authoritative and the client is assumed hostile.** A client
   sends *intents* ("I am holding north-west", "cast spell 7 at this point");
   it never sends state. Position, health, loot, currency, and progression are
   the server's, computed from intents it validated. Every field off the wire
   is bounds-checked before it reaches a rule (§5.4, §26.4). There is no trust
   level a client can reach that shortens this path.
2. **The world is a pure function of its seed, so terrain is never
   transmitted.** `wintersun/world` answers any chunk from `(realm_seed,
   chunk)` alone, identically on every Tier-1 target. The client generates the
   ground it walks on; the server sends only what the seed cannot predict —
   entities, and the stored deltas players caused. This is what makes a large
   world affordable in bandwidth and in RAM (§26.6, §26.7).
3. **What the seed may not decide, the server keeps secret.** Terrain is
   public: a player can see the hills anyway, and a client-side generator
   leaks nothing. Anything whose value *is* its concealment — a dungeon's
   interior layout, an unopened container's contents, an undetected trap's
   position, another player outside your awareness radius — is generated and
   held **server-side only** and streamed under interest management. A design
   that lets the client derive a secret from the seed is a defect.
4. **The authoritative simulation is deterministic across all four Tier-1
   targets, and that is a test.** It uses IEEE-754 `f64` with the basic
   operations and `lib/util::mathf` — TAIRiX's own libm — and nothing else.
   Because the transcendentals are first-party Rust rather than a per-platform
   libm, the same source yields the same bits on `x86_64`, `aarch64`,
   `riscv64` and `wasm32`; Rust contracts no FMA, so `a * b + c` stays two
   operations. WS2 and WS3 each carry a vertical that runs a fixed seed for a
   fixed tick count on every target and asserts one state hash. Reaching for
   any other maths in an authoritative path breaks this and is refused.
5. **Content is data, and there is no scripting language.** Spells, items,
   skill trees, archetypes, loot tables, biome parameters, weather fronts, and
   dialogue are declarative documents validated at load against a closed
   vocabulary of effects. TAIRiX ships no interpreter, no bytecode VM, and no
   JIT for game content: that would be an untrusted-code execution surface
   needing `CAP_JIT_MAP_EXEC` (§19.2), and a closed effect vocabulary reaches
   the same expressiveness without it. Adding an effect is adding a variant
   with its rule, its test, and its documentation.
6. **The renderer is software, first-party, and complete on its own.** It
   draws through `lib/raster`'s one anti-aliased scan converter and blend
   (§2.2 — no second rasteriser), parallelises over `lib/parallel`'s
   `JobRunner`, and selects SIMD kernels through `lib/cpuops`. It is the
   mandatory always-available path, exactly as the software `Display` path is
   for the desktop (§17.3). GPU offload (WS19, P7) accelerates it behind one seam;
   it never becomes a second renderer, and the game is fully playable without
   it.
7. **The game is an ordinary app with an ordinary manifest.** It holds only
   what it asks for and is granted: `CAP_SHM` for its window surface, `CAP_NET`
   to reach a realm, `CAP_FS_ACCESS` for its own bundle reads, and
   `CAP_SANDBOX_SPAWN` for the decode workers. It requests no capability the
   desktop's other apps do not, and it introduces **no new capability at all**
   — a realm's own roles (player, moderator, administrator) are the realm's
   records enforced by the server, not kernel authority, because they govern a
   game's objects and not the machine's (§5.2).
8. **The realm is three processes, not one.** A gateway holding client
   sockets, one or more zone shards simulating regions, and a single store
   process owning the database. Separate address spaces mean a zone fault
   cannot take the realm down or reach the player records, and one writer
   means the database needs no distributed commit. This is the microkernel
   decomposition applied to a game server, and it is what makes the
   thousand-player target defensible rather than asserted.
9. **Interactive surfaces obey §28 without exception.** No store read, no
   file read, no IPC round trip on the frame loop; a settings slider changes
   the in-memory model and repaints, and writes once when it settles; a paint
   reads nothing; a burst of pointer motion produces one frame. The character
   designer and the detail-level slider are the two surfaces most likely to
   violate this — the slider being the charter's own worked example — and both
   are specified against it explicitly (WS17, WS18).
10. **`abi-v1` is not frozen, and the game does not touch it anyway.** The
    game's wire protocol is the game's own, in `wintersun/net`, held to the
    `lib/abi` *discipline* — versioned, fixed-width, bounded decode, fail
    closed, fuzzed — but not in `lib/abi`, which is the user/kernel contract
    (§9). The one exception is P3's window size states, which are genuinely
    desktop ABI and land in the desktop's plan.

## 1. Where each piece lives, and why there

**A game is not an OS library, and none of it goes in `lib/*`.** `lib/` is the
OS's shared-library namespace; a game's world generator, combat rules and wire
protocol have no business in it, whatever the build graph would make
convenient.

The convenience in question was real, and it is worth naming so it is not
re-discovered: five separate userland programs — the client, the three realm
server binaries, and the admin command — share one simulation, and
`cargo xtask deps-check` forbids a `userland/*` crate from depending on another
`userland/*` crate (§17.4). That constraint is satisfied by giving games their
own **layer**, not by moving game code into the OS libraries.

`userland/games/` is therefore a leaf subtree modelled exactly on
`userland/gui/`, which already does this and is already enforced: its crates
compose each other and `lib/*`, and **nothing outside the subtree may depend on
them**. So game code cannot leak into the OS even by accident — the check fails
the build.

```
userland/games/wintersun/
├── app/      # the client `Run` + the three realm server binaries → WinterSun.app
├── ctl/      # wintersunctl — the admin command bundle
├── art/      # material synthesis, the splat field, decals, particles, palette
├── figure/   # rigs, sockets, pose clips, blending, motion layers, presets
├── net/      # the realm wire protocol and session handshake
├── rules/    # the authoritative simulation and the game rules
└── world/    # the seed-pure procedural world generator
```

The split into crates is not decoration: it makes the boundaries the build
enforces rather than the reviewer. The server binaries cannot reach the render
code because they do not depend on the crate that holds it, and `ctl` reaches
only `net`.

**WS1 adds `Layer::UserGame` to `tools/xtask/src/commands/deps_check.rs`** —
`classify` gains a `userland/games/` arm *before* the generic `userland/` arm,
`layer_allows` gains `UserGame => matches!(to, Lib | UserGame)`, and the
existing no-reverse-dependents check is extended to the subtree. That ordering
matters: the arm must precede the generic one or every game crate classifies as
plain `Userland` and its internal edges are refused. The failure mode if the arm
is ever removed is the *safe* one — game crates fall back to lib-only and the
build complains — which is why the subtree is nested under `userland/` rather
than made a new top-level tree, whose fallthrough in `classify` is
`Layer::Tooling` and therefore exempt from layering altogether.

**What this work adds to `lib/*` is only what the OS itself wants:**
`lib/recdb` (`plans/RECDB.md` — the record store whose other consumers are the
account database, the journal index and the app-data blob index), `lib/gpu`
(`plans/GPU.md` — whose other consumer is the compositor), and a `shape` module
in `lib/raster` holding the parametric outline primitives that `cinder` and
this game genuinely share (`plans/FIGURE.md` FG1). Nothing else.

```
userland/games/wintersun/app/     # /System/Applications/wintersun.app
├── AppInfo                       #   signed manifest: kind = application
├── Run                           #   the client (and the listen-server host)
├── Code/wintersund               #   the dedicated gateway
├── Code/wintersun-zone           #   a zone shard worker
├── Code/wintersun-store          #   the realm's single database writer
├── Resources/                    #   the icon, the figure presets, content documents
└── Help/<locale>/                #   structured-Markdown help, en-US mandatory
```

Everything with behaviour worth testing is in the non-binary crates; `app/` and
`ctl/` only compose — the pattern `userland/apps/sapper` and
`userland/apps/cinder` already follow, and the reason `cinder`'s frame advance
was moved out of its `Run` binary (`plans/CINDER.md` B3a: a freestanding binary
is reachable by no host test, which is how a companion that walked on the spot
survived a green pipeline three times).

**The bundle is self-contained** (§16.5). The `Run` binary, every `Code/`
binary, the manifest, the figure presets, the content documents, the icon, and
the help tree are real files inside `WinterSun.app`. Nothing is compiled into
the kernel or the image builder, and no central list of content exists: the
content documents are discovered by scanning `Resources/`, exactly as drivers
are discovered from their bundles (§16.5, §18.6).

**A realm is started, not installed.** `Run` with no realm hosts one locally
(it spawns the three server binaries and connects to itself over the loopback
path, so the single-player and multiplayer code paths are the same code —
there is no offline mode to keep in sync). `wintersund` is the dedicated form
for a machine that serves only.

## 2. WS2 — the world

A realm is a `u64` seed and a small parameter document. Generation is a
pipeline of pure stages over a chunk grid; each stage reads its inputs at a
coarser scale than it writes, so a chunk needs only a bounded halo of its
neighbours and never the whole world.

**"A coarser scale" is one scale, fixed, and global.** Stages 1–5 and 7 are
not local questions — discharge depends on the whole upstream basin, a rain
shadow on everything the wind crossed, a road on reaching the town at its far
end — and a window *centred on the asking chunk* gives a different answer per
query, which is a river that flows uphill across a seam. So they are solved
once over a **realm field**: a coarse grid of a fixed sample count, global and
exact, and therefore seam-free by construction rather than by a halo that
happens to be wide enough. A fixed sample count, never a step in world units,
is also what keeps its cost the same for a realm four chunks across and one
four thousand chunks across.

The chunk stage is the fine one: it reads the realm field, adds everything
below the coarse step, and depends on nothing outside a fixed ring of cells.
Only one quantity it computes has a neighbourhood dependence at all — the
shore distance, a distance transform — which is why the ring exists and why
its radius is that band's width. Scatter (6) is inherently fine and runs
there, last, because it reads the structure stamp: nothing grows on a road.

1. **Uplift.** Continental plates as a Voronoi partition of the sphere-mapped
   plane with per-plate drift; boundary convergence gives mountain belts,
   divergence gives rifts and inland seas. This is what stops a heightfield
   looking like noise: ranges get a direction and a reason.
2. **Relief.** Multi-octave gradient noise, domain-warped, amplitude-shaped by
   the uplift field, with ridged octaves inside mountain belts and billowed
   octaves in dunes. All from `lib/rng`'s deterministic non-cryptographic tier,
   keyed per stage so adding a stage cannot shift an earlier one's output.
3. **Hydrology.** Flow direction and accumulation over the relief; channels
   where accumulation crosses a threshold, widening downstream — streams
   become rivers become estuaries. Lakes fill depressions to their outflow.
   Then a bounded pass of hydraulic erosion and sediment deposition, which is
   what cuts valleys the roads later follow and lays the flats the settlements
   later use. Rivers carry a discharge value, so a ford is possible where a
   bridge is not needed.
4. **Climate.** Temperature from latitude, altitude, and a continentality term.
   Moisture advected from water bodies along prevailing winds, with orographic
   lift on windward slopes and a rain shadow behind — which gives a desert a
   reason to be where it is.
5. **Biomes.** A Whittaker classification over (temperature, moisture) with
   altitude and slope overrides, producing not a label but a **normalised
   weight vector** over the material set. A biome boundary is therefore a
   gradient, and §3's splatting draws it as one. WinterSun's set is cold-biased
   and coherent with its name: boreal forest, snowfield, glacier, tundra,
   fell heath, cold steppe, temperate forest, moor, saltmarsh, ashland,
   and the rift-scarred waste where the world was torn.
6. **Scatter.** Vegetation, rocks, and resource nodes placed by Poisson-disk
   sampling weighted by biome, slope, and moisture, so nothing grows on a cliff
   and a forest has spacing rather than clumps.
7. **Sites.** Settlements on flat, watered, defensible ground near a
   confluence or a coast; then roads as least-cost paths (A* over a traversal
   cost field that prefers valleys and level ground, pays to cross a river, and
   reuses an existing road — so roads converge and braid like real ones);
   then dungeon and shrine entrances, ruins, and rift scars. A site's
   *entrance* is world data; its *interior* is server-only (decision 3).

**Scale.** A chunk is generated on demand, cached in a `lib/reclaim`-governed
cache sized from discovered RAM, and dropped under pressure — never stored,
because it is cheaper to recompute than to page in, and never resident in
bulk. Player-caused changes are the only world state that persists, as sparse
deltas in the store, applied over the generated base. A realm's world is
therefore O(changes) on disk and O(working set) in RAM regardless of its
extent, which is what §26.7's floor demands.

**Generation is incremental and interruptible.** A chunk's stages are
individually bounded and resumable, and generation runs on `lib/parallel`'s
runner (`Serial` where there is no second core, which is not a stub but the
correct runner). A client never blocks a frame on generation: a chunk not yet
ready draws as the coarse relief the previous stage already answered (§28.5 —
a paint reads nothing and draws a meaningful placeholder for what has not
arrived).

**What WS2 now guarantees.** The realm field solves plates, relief with a
sea-level cut that honours the requested submerged fraction, Priority-Flood
drainage with stream-power incision and hillslope diffusion, climate by wind
advection, settlements, minimum-spanning-tree roads routed by integer-cost
A\* that reuses existing road, and landmark entrances. The chunk adds detail
relief, the channel carve, the structure stamp, the climate correction, the
Whittaker blend and the scatter. Determinism is staked on one constant,
`digest::REFERENCE_DIGEST`, asserted by the host suite and by one vertical per
Tier-1 target (`tests/integration/world_determinism_qemu_{aarch64,riscv64,
x86_64}` and `tests/integration/world_determinism_wasm32`, the last under a
plain WebAssembly engine because its subject is arithmetic and a browser would
narrow where it can run). The crate decodes no bytes — a parameter document's
wire form belongs with the protocol in `wintersun/net` — so it has no
untrusted-input parser and no fuzz target of its own.

`Facing` gained `unit_vector` in `wintersun/net`, with the type, because the
wind is the first consumer to need an angle convention and two crates picking
opposite ones would be a defect neither could see.

**Where `lib/parallel` attaches, and why not inside this crate.** The
parallelism worth having is over *chunks*, not within one: a chunk's phases
are sequential by dependency, so a runner inside the build would have nothing
to overlap. `ChunkBuild` is therefore exactly the unit a `JobRunner` runs —
`for_each` over a slice of them, one `step` per visit — and the composition
belongs to the client that owns the runner and the frame budget (WS5). Adding
the wrapper here before that caller exists would be speculative surface, and
each build is already independent of every other, so the composition needs
nothing from this crate that it does not already have.

The *simulation's* determinism vertical is a different claim over a different
subject and lives with the code that ticks (§5).

## 3. WS4/WS5 — what it looks like

### Texture splatting, not tiles

Terrain has no tile grid. Each terrain sample carries the biome stage's
normalised material weights, and a pixel is the weighted blend of its
materials — but blended by **height-offset weighting**, not linearly: each
material carries a height field, and the material whose (weight + height)
is greatest wins most of the pixel. That is what makes gravel emerge through
grass in patches rather than fading into a grey average, and it costs one extra
texture read.

- **Materials are synthesised, not shipped.** A material is a parameter set —
  base and variation colours, grain scale, height octaves, roughness — from
  which `wintersun/art` generates its texture and height at load, once per
  (material, mip), into the reclaim-governed cache. Resolution-independent,
  a few hundred bytes on disk, deterministic, and it sidesteps shipping
  megabytes of photographic tiling.
- **Repetition is broken by construction.** Material lookups are offset by a
  low-frequency rotation/scale jitter keyed on world position, so a large
  grassland does not visibly repeat.
- **Roads, rivers, and scars are decals in the weight field, not geometry.**
  A spline stamps its material weights with a soft falloff, so a road *wears
  into* the grass with frayed edges, a river bank grades through mud to
  shingle, and two roads meeting merge rather than overlap.
- **Shipped raster masters are legitimate only where artwork is a picture**
  (`AGENTS.md` §10): the loading art, item icons, and portraits are raster masters; all
  chrome is SVG; every material and every figure is procedural. Any shipped
  asset is decoded in a §19.5 sandbox under a fixed byte bound and falls back
  to a built-in tier — the game does not get its own decode path
  (`plans/ICONS.md`).

### What WS4 settled

The palette, the material set, the splat kernel, the decal stamp and the
particle vocabulary are built, in `wintersun/art`. What a later item needs to
know:

- **The crate contains no floating point at all**, and
  `deny(clippy::float_arithmetic)` makes that a compile error. Value noise,
  smoothstep, the blend, distance-to-segment and particle advection are all
  shifts, masks, byte-wide weighted means and one exact integer square root.
  So bit-identity across targets **follows from the language** rather than
  from a test, and this crate therefore carries **no four-target QEMU
  vertical** where WS2 and WS3 each carry one — four emulated machines would
  be confirming Rust's integer semantics, not the code.
  `digest::REFERENCE_DIGEST` exists for a digest's other job (an unintended
  change to the art shows up as a moved number) and **WS5's client frame
  vertical folds it in**, which is where the cross-target rendering claim
  belongs: over a whole composited frame, not one crate.

  **Two things WS5 owes this crate**, because nothing consumes it yet and so
  nothing in the gate reaches it beyond the host suite, the proptest model and
  clippy: the client vertical **folds `digest::REFERENCE_DIGEST` in**, and it
  is what first pulls `wintersun/art` into a build for each Tier-1 target.
  All four targets were confirmed to build at WS4
  (`wasm32-unknown-unknown`, `aarch64-unknown-none`,
  `riscv64gc-unknown-none-elf`, `x86_64-unknown-none`), but by hand rather
  than by the gate, and a hand check does not stay true.
- **The weight field is one mechanism with one mutation.**
  `WeightField::cover` is the *over* operator on a weight vector, and
  everything that changes the ground goes through it: road and river decals
  now, WS13's snow accumulation and WS9/WS10's scorch marks later. Covering
  takes the maximum rather than the sum, which is exactly what makes two
  roads merge — a second stamp at the same coverage is a no-op — and a stamp
  lighter than every material already on a full field is refused rather than
  displacing something heavier.
- **A span is the unit, not a pixel.** Everything a pixel needs beyond its
  own texel read is linear along a horizontal run inside one cell row, so a
  caller does the *vertical* interpolation (one `WeightField::lerp` per cell
  row per raster row) and `splat::splat` steps the horizontal. That turns
  four hash evaluations per pixel into four per span, and it is what the
  budget assumes.
- **Resolution is total, so the pass never fails.** A tile the cache will not
  admit degrades to a coarser mip and then to the material's flat mid tone at
  its standing height. `MaterialCache::ensure` reports residency and
  `peek` reads it, deliberately as two calls: a splat needs four tiles at
  once and four live borrows cannot come out of four mutable calls — and it
  is the ask-then-paint shape an interactive loop wants anyway.
- **The degradation ladder's first two rungs exist here.** Particle density
  is `area / pressure band`, and `material::Quality` is the octave knob that
  is also the tile cache's generation token. The remaining rungs (light
  buffer, shadow softness, render scale) are WS5's, and `Quality` is where
  the material rung is turned.
- **No `lib/cpuops` family yet, deliberately.** A family with one portable
  candidate selects nothing, and reaching for per-architecture intrinsics
  before a measurement says the portable kernel misses its budget is the
  speculative optimisation the charter forbids. The measurement is M1's exit
  criterion. The kernel is already shaped as the contiguous span function such
  a candidate would replace, so adding one later is adding a candidate, not a
  reshape.
- **No fuzz target, because there is no decoder.** Material rows are compiled
  in, weight fields come from the generator's own output, and a decal path is
  either that generator's road or a player-caused change `wintersun/net`
  already bounds-checks and fuzzes. The adversarial coverage is the proptest
  model, enrolled as `wintersun-art`.
- **A material's standing height is an art-direction statement**, because the
  relief is what decides which material wins a shared pixel: rock above
  gravel above sand above water, glacier above snowfield. A river bank grades
  through mud to shingle because shingle stands higher, not because anything
  special-cases a bank.
- **`lib/raster::shape` (FG1) was not needed by WS4 or WS5.** Decals are
  polylines stamping weights and particles are points; neither wants an
  outline primitive. It is built now, as WS6's prerequisite.

### What WS5 settled

The client shell is built, in `wintersun/app`: the `[lib]` holds the camera,
the ladder, the render target, the terrain lattice and its splat, the light,
the tiled frame, the pacing, the input drain, the size-state model, the budget
governor and the frame digest; the `[[bin]]` is the bundle's `Run` and only
composes them. What a later item needs to know:

- **The frame budget was measured, and it holds on the reference machine.**
  At 1280×720 on four threads: terrain **4.1 ms** against its 5.0 ms
  allocation (81%), light **1.7 ms** against 2.0 ms (87%), 5.8 ms of drawing
  in a 16.6 ms frame, a 3.49× speedup over one thread. The number this plan
  called "the single most likely to be wrong" is right. `tests/budget.rs` is
  the measurement and prints it; it asserts the *frame*-level claim rather
  than each pass's own allocation, because the host is not the reference
  machine and a per-pass assertion there would be measuring the machine.
  - **Open: the frame assertion cannot hold under a parallel workspace run,
    whatever the renderer costs.** It takes a wall-clock reading over a
    four-thread runner while `cargo test --workspace` has ~20 other test
    binaries on the same cores, so the figure it asserts against is a
    property of what else is running. On a development host about 2.5×
    slower per core (and reaching a 2.5× thread speedup, not 3.49×) it
    measures terrain ~9.7 ms and light ~5.1 ms — roughly 2× and 2.5× their
    allocations — for ~15 ms of drawing in the 16.6 ms frame: it passes run
    alone, with ~20% variance, and fails in the suite. That is the
    load-dependent wall-clock assertion the charter names, so it is a
    defect in the *instrument* as well as a renderer that is over budget on
    a slower machine, and re-running until it passes settles neither.
    Settling it means one of: normalising against a reference the
    measurement takes itself, asserting the shape the module doc already
    describes while tracking the absolute figure outside the gate, or
    holding the budget on the slower host with the suite loading it.
  - **Three output-identical optimisations have already been taken**, so
    they are not re-derived: `FastHash::hash_bytes` is `#[inline]` (the
    noise lattice hashes a fixed 16-byte key, and folding the length at the
    call site removes the slice walk — the hash was ~18% of the whole
    profile); the blend's three per-pixel channel divisions are an exact
    reciprocal table over the bounded divisor, proven exhaustively by
    `reciprocals_are_exact`; and `shade` skips the texel fetch for any slot
    whose weight is too far under the heaviest to reach the blend floor
    however tall its relief, proven by
    `a_skipped_slot_could_not_have_reached_the_floor`. The light pass's
    composite walks texel-wide runs instead of dividing per pixel. What
    remains is the noise: the warp's four lattice corners are re-hashed per
    span, and adjacent spans in a row share a warp cell, so memoising the
    corners is the next real gain and the one that needs a design — the
    field is read through `&Warp` from every worker.
  - The light pass was **53% over budget** on its first measurement, entirely
    because its buffer was shaded on the calling thread while only the
    composite was distributed. Shading its texel rows through the same runner
    brought it to 87%. That is the whole reason the measurement exists.
- **A tile is a full-width band of rows**, not a square. Every pass steps
  horizontally — the splat walks a span inside one cell row, the composite
  walks a row of the buffer — so a vertical cut would divide the unit each is
  built around. The per-tile bucketing a later item does is unaffected.
- **The camera clamps where the view is projected, not where it is aimed**,
  and carries the realm's extent to do it with. Clamping on being aimed is
  correct until the window grows, at which point the wider view reaches past
  an edge the camera had already settled against. The proptest model found it;
  `look_at` now records a wish and `centre(w, h)` settles it.
- **Shading is relative to the palette.** A slope facing neither way draws the
  material's own colour. A plain multiply by a tint darkens every surface in
  the world by whatever the tint's mid-point is, which is a palette change
  wearing lighting's clothes. The relief term saturates at
  `MAX_STEP_RISE_SUB_UNITS` — the rules' own slope/cliff line — so ground a
  player can walk over is shaded across its whole range.
- **The ladder's rung 4 turns the relief-shading stencil** (two cells, one
  cell, none) until WS6's figures bring cast shadows for it to also govern.
  The wider stencil is both the penumbra and the dearer, so narrowing it
  before dropping the term is the right order either way.
- **A paint reads nothing.** Chunk generation is handed to a worker through
  the shared deferral desk; the frame draws the ground that has arrived and
  marks the rest. The desk holds one request, which is the right policy: the
  nearest missing chunk is always the best thing to be solving, and a
  displaced ask is simply re-made next frame.
- **The two debts to WS4 are paid.** The client vertical folds
  `tairix_wintersun_art::digest::REFERENCE_DIGEST` in, and
  `client_frame_qemu_{aarch64,riscv64,x86_64}` plus `client_frame_wasm32` are
  what first build the ground art for each Tier-1 target — by the gate, not by
  hand.
- **`lib/raster` gained `pixels_mut` and `resample_into`.** The renderer
  writes the window's own pixels at native scale rather than composing a frame
  and copying it, and resamples into a destination the caller holds rather
  than allocating a screen-sized surface per frame on the path a machine
  reaches precisely because it is short of time.
- **`world::chunk::ChunkWindow` is the one sorted-window lookup**, hoisted out
  of `rules::ChunkTerrain`, which now wraps it. The client needs a chunk's
  blend and the simulation needs its heights; both were binary-searching the
  same slice the same way.
- **The library takes `tairix-parallel` with `default-features = false`**, as
  `lib/raster` does: the pool creates threads through `lib/rt`, which brings a
  global allocator and a panic handler, and a bare-metal *consumer* of the
  library — each of the four verticals — supplies both itself. The binary's
  runtime sits behind the default `run` feature for the same reason.
- **Still no `lib/cpuops` family.** The portable kernel makes its budget, so
  adding per-architecture candidates now would be the speculative optimisation
  the charter forbids. The splat is still shaped as the contiguous span
  function such a candidate would replace.
- **No fuzz target**, for WS4's reason: the client decodes nothing untrusted.
  Its adversarial coverage is the proptest model, enrolled as `wintersun-app`.
- **What WS5 deliberately does not draw**: figures (WS6), particles and
  weather (WS13), the console and chat (WS15/WS16). The frame's pass order and
  the budget name them now so each lands in a place that is already measured.
  The camera follows an ordinary `rules` entity walking real collision ground,
  so the body is there before the art for it is.

### Lighting, and why the name matters

WinterSun is lit by a low sun. That is an art direction and a rendering
simplification at once: a single directional light at a shallow angle gives
long directional shadows, strong rim light on north faces, and a cold-to-warm
gradient across a slope, all of which read at top-down scale and all of which
are cheap. Terrain is shaded by its slope normal against the sun; entities and
scenery cast soft projected contact shadows squashed along the light direction
(the readable-jump trick `cinder` already uses). Night is the same pass with a
moon and point lights from lanterns, fires, and spell effects, accumulated into
a light buffer at half resolution and upsampled.

### The frame

A tiled, threaded software renderer. The visible area is split into tiles; each
tile is a job on `lib/parallel`; within a tile the passes are terrain splat →
decals → ground scenery → entities depth-sorted by ground y → overhead canopy
→ particles → weather → light/fog composite → UI. Scenery and entities are
bucketed per tile once per frame, so a tile touches only what overlaps it.

Per §28: input is drained, then the frame is produced once from the state the
events left. The simulation runs at a fixed tick; the render interpolates
between the last two authoritative states, so motion is smooth at any display
rate and the sim rate is not a visual property.

### The frame budget, in numbers

"Playable" is not a budget, and a renderer without one cannot be reviewed. The
baseline target is **1280×720 at 60 Hz — a 16.6 ms frame — on a four-core
reference machine**, with the per-pass allocation below. These are budgets to
be *measured* at M1, and a blown budget is a defect fixed or reverted in the
same change, exactly like a failed test (§2.16).

| Pass | Budget |
|---|---|
| Terrain splat (material blend + detail) | 5.0 ms |
| Ground decals, scenery, entities and figures | 3.5 ms |
| Particles and weather | 2.0 ms |
| Light, fog and atmosphere composite | 2.0 ms |
| UI and overlays | 1.0 ms |
| Headroom (present, input, jitter) | 3.1 ms |

Concurrent budgets: ≤256 visible entities, of which ≤64 carry a full rig; the
simulation runs on its own cadence and is **not** inside the frame budget.

**Quality degrades in a stated order, and on `auto` the frame rate is never
what gives way.** When a frame overruns, the renderer sheds in this sequence
and no other: particle density → light-buffer resolution → detail-material
octaves → shadow softness → render scale (with upscale to the window). The
order is fixed so degradation is reproducible and reviewable rather than an
emergent surprise, and the active step is observable for diagnosis. A machine
with headroom scales *up* to the display's native resolution, capped at
2560×1440 for the software path.

**`auto` is the default, and it never sheds a detail the player needs to
read.** The ladder has a floor, and the floor is the last notch whose frame
still passes the readability checks `plans/FIGURE.md` FG5 defines — the
silhouette coverage band, the landmark count, the contrast ratio — taken at the
figure's drawn size. Two rungs are pinned by it concretely: a contact shadow
stops at `Hard` and never reaches `Off`, because the shadow is what says where
a figure stands and whether it is airborne; and the render scale stops at the
coarsest fraction whose attack telegraphs and figure silhouettes still clear
the checks. The floor is therefore measured off the art rather than chosen
here, and it moves when the art does.

It is measured **once, at build time**, by the FG5 contact-sheet harness that
already renders every preset at every drawn size, and compiled in as the
ladder's floor. Nothing measures readability on a frame: that would put the
most expensive check in the project on the hot path to decide whether the
frame is too expensive.

Reaching the floor with the frame still over budget is **reported, not
hidden**: the frame rate gives way, the diagnostic names the floor as the
reason, and the player is told a forced level exists. A renderer that quietly
crossed the floor to hold 60 Hz would be trading away precisely what the player
needs to see in order to keep what they would not notice.

**A forced level is the player's own choice and holds regardless of frame
time** — that is the whole point of it — and it may go below the floor, because
they asked for it. There the frame rate is what gives way, by their decision
rather than the renderer's. The surface, its presets, and what the sliders
offer are WS18.

The frame digest folds a frame at each end of the ladder (`0` and
`Ladder::MAX_STEP`), so neither the governor nor a player's setting can move
the cross-target claim; adding a rung changes `MAX_STEP` and therefore the
digest, which is the intended coupling rather than a nuisance.

The honest risk: a 720p frame is 0.92 M pixels, and a terrain pixel touches
several material samples. The budget above assumes SIMD kernels selected
through `lib/cpuops` and tiles distributed over `lib/parallel`, and it is the
single most likely number in this plan to be wrong. That is precisely why M1
exists and why its exit criterion is this measurement.

### WS13 — weather, sky, and the day/night cycle

Weather is **server-authoritative, seeded, and regional**. Fronts are moving
systems of pressure, moisture and temperature advanced on the tick over the
realm's map, so it can rain in one valley and be clear over the next ridge, and
every client in a region sees the same storm at the same tick. A client is sent
its region's weather state — cloud cover, precipitation kind and intensity,
wind vector, fog density, electrification, temperature — and interpolates it;
every transition is a ramp, never a switch, because weather that changes
instantly is the tell that it is decoration.

- **Sky.** Sun and moon altitude derive from the world clock (`Time64`, with
  day length a realm parameter), giving the gradient, the disc and its halo,
  the horizon haze, and a star field that rotates with the clock. WinterSun's
  low sun is the art direction: long shadows and a cold-to-warm slope gradient
  all day, which is what reads at top-down scale.
- **Clouds.** Two or three advected noise layers at different altitudes and
  speeds, so they parallax; lit by the sun's angle, so undersides darken as a
  front builds. Their shadows project onto the terrain as a moving multiply
  mask — cheap, and the single most convincing atmospheric cue available.
- **Precipitation.** Rain, sleet, snow and hail as depth-layered particle
  fields advected by the wind vector, streaks oriented to wind and camera
  motion, with the particle count derived from the visible area and the memory
  pressure band rather than a fixed constant (§24.1, §26.3).
- **Snow settles through the material system, not a new one.** Accumulation
  raises the snow material's weight in the splat field, so it covers ground
  through the same height-weighted blend everything else uses, drifts against
  obstacles, and melts on a temperature-driven timer. Reusing the splat field
  is why snow costs no second mechanism (§2.2).
- **Wetness reuses the hydrology.** Rain raises a wetness term that darkens and
  glosses the material blend and pools in hollows using the flow-accumulation
  field WS2 already computed — so puddles form where water would actually go.
- **Fog.** Distance and height fog composited in the light pass, ground mist
  pooling in hollows at dawn and heavier over marsh biomes.
- **Lightning.** A strike selects a ground point within an electrified region;
  the bolt is a jittered branching polyline; the flash raises scene luminance
  for a few frames. The flash is **intensity-capped and separately
  adjustable**, because an uncapped full-screen white flash is a
  photosensitivity hazard, not an effect (WS18). Thunder is queued as a sound
  delayed by the real speed of sound over the strike distance and low-passed by
  it (§9), which is the detail that makes a storm have depth.
- **One wind vector, consumed everywhere.** It drives cloth and hair springs,
  foliage sway, rain angle, particle advection, and the audio bed's character —
  one definition, never a per-consumer copy (§2.2).
- **Weather affects the rules, so it is not merely drawn.** Visibility narrows
  detection ranges, a blizzard drains warmth and stamina, lightning can strike
  a character, and heavy rain quenches fire effects. Because weather is
  authoritative and seeded, those effects are identical for every player and
  cannot be turned off by a client that dislikes them.

## 4. WS6 — characters

The figure engine is `plans/FIGURE.md` (P6); what the game adds is its own content:
humanoid and creature rigs, the species/archetype presets, the clip set (idle,
walk, run, dodge, melee light/heavy, draw/loose, cast/channel, hit, stagger,
fall, die, sit, swim, climb), and the state machine that selects and blends
them. The approach is `cinder`'s, generalised and proven: parametric parts on a
skeleton, pose parameters as data, one body frame so a single rig serves every
heading without per-direction sprite sets, and procedural layers (gait phase
from velocity, look-at, weapon recoil, cloth and hair sway, breathing) over the
authored clips.

Equipment is parts, not paint: a helm, a pauldron, a cloak, a blade are parts
attached to named sockets in the rig with their own palette, so a character's
gear is visible, mixable, and costs no new art path.

## 5. WS3/WS9/WS10/WS11 — the simulation and the rules

### The tick

The authoritative step is fixed-rate (**30 Hz default**, a realm parameter) and
total: it consumes validated intents, advances every entity, resolves
interactions in a fixed order, and emits the deltas. Order is deterministic and
documented; "whatever order the map iterated" is a defect. Movement is
validated against the mover's own speed and the collision field, so a client
claiming an impossible step is corrected, not believed.

30 Hz rather than 20 because this is an action RPG with dodges and aimed
shots: a 20 Hz tick quantises every input to 50 ms, which is enough to make a
dodge feel unreliable in a way no amount of client-side polish hides. **An
intent additionally carries the client's sub-tick sample time**, so the server
places an action *within* the tick it arrived in rather than snapping it to the
boundary — recovering most of the remaining granularity for the cost of one
field.

### What WS3 settled

The tick, the collision field, the broad phase, the stat curves, the damage
pipeline and the status vocabulary are built, in `wintersun/rules`. What a
later item needs to know:

- **The step order above is fixed and documented on `Zone::step`.** Bodies
  are iterated in identity order out of an array kept sorted by an identity
  the zone mints monotonically; nothing reads a hash order anywhere.
- **The simulation is integer arithmetic throughout but for one heading
  conversion** (`Facing::towards`, which went into `wintersun/net` beside
  `unit_vector` for the same reason that one did). Movement is fixed-point
  with a carried remainder per body, separation is an exact integer square
  root. Anything folded into the digest has a fixed width, never `usize`,
  whose four bytes on `wasm32` would otherwise put the host's pointer size in
  the answer. Reaching for `f64` in an authoritative path is a defect.
- **The claim is staked on `digest::REFERENCE_DIGEST`**, a scripted session
  folded tick by tick — not a final state, so a divergence that later
  converges is still caught. Asserted by the host suite and by
  `tests/integration/rules_determinism_qemu_{aarch64,riscv64,x86_64}` and
  `rules_determinism_wasm32`. It is a *separate* constant from WS2's, and the
  session walks a pattern rather than a generated realm, so a change to
  either cannot make the other's evidence ambiguous.
- **The collision seam is a data source, not a policy.** `Terrain` answers
  ground height and water height per cell; the passability rules live in the
  crate. Fetching chunks is a cache with a budget and belongs to the process
  holding it, so `ChunkTerrain` borrows a sorted window and owns no cache.
  Absent ground reads as impassable.
- **A root or a stun works through the speed, never by refusing the input.**
  Refusing would leave a stale held direction to resume when it expired, so a
  body that changed its mind while held would walk the old way. Discrete
  actions a stun or a silence forbids *are* refused, with the reason.
- **An intent naming an action, spell, item or interaction is refused as
  unresolvable**, because no table exists to resolve one. That is the final
  code path, not a placeholder: WS9–WS11 add the tables the same lookup will
  then find. The damage, healing, status and resource verbs those items will
  call are built and tested on `Zone`.
- **Bounds are fixed ranges rules are defined over, not capacities.** They
  also bound every product the pipelines form, which is what lets the whole
  simulation run in checked integer arithmetic with no saturating step hiding
  a real overflow. Entity, event and refusal storage grows on demand and
  fails closed as a typed error.
- **`lib/parallel` is still not wired here.** A tick's phases are sequential
  by dependency and its bodies share one table; the parallelism worth having
  is over zones and over chunks, which is WS5's and WS8's composition.

### Combat (WS9)

Stats are a small closed set with documented curves. Damage is
`base → attacker modifiers → defence and resistance → status interaction →
floor`, each step a pure function with its own test, and the whole pipeline
host-tested against hand-computed cases. Melee is a swing arc resolved against
capsules; ranged is a projectile with real flight (speed, gravity where it
applies, and a per-tick swept test), server-resolved and client-predicted for
the shooter's own shots only. Traps are entities with a trigger volume, an
owner, a detection difficulty, and an arm/disarm state; an undetected trap is
not sent to the client that has not detected it (decision 3).

Archetypes — the "player types" — are data: a stat curve, a starting skill
tree, gear and school permissions, and a resource model (stamina, mana, focus,
or a pair). Adding one is a document plus its tests.

### Magic (WS10)

A spell is a declarative document: school, cost, cast time, whether it is
instant, cast-then-release, or channelled; its shape (self, touch, projectile,
cone, circle, line, aura); its targeting rules; its effect list; and its visual
and audio descriptors. Effects come from the closed vocabulary — damage, heal,
resource change, status apply/remove, displacement, summon, terrain or material
change, light, reveal — each with a rule and a test. Interruption, line of
sight, friendly fire, and diminishing returns on repeated status application
are rules, not per-spell code.

Visual effects compose from the same particle and decal vocabulary as weather:
a frost spell lays a real ice material decal that the terrain splat then
blends, and it melts on a timer. That is why effects sit in `art` beside
materials rather than in a separate effects system.

### Game feel — the part that decides whether it is any good (WS9–WS11)

A correct combat simulation that feels mushy is a failed action RPG, and feel
is not something that can be added afterwards: it lives in the same numbers the
server enforces. So it is specified here, in data, and tested.

- **Every action is windup / active / recovery, authored as frames.** An
  action document states the three durations, and **the animation clip's timing
  is derived from them** rather than authored beside them — so tuning a number
  changes the feel and the visuals together and they cannot drift apart. A clip
  whose length disagrees with its action is refused at load.
- **Animation events drive the simulation, not the reverse.** A clip carries
  named events at phases — `footstep`, `hit_frame`, `loose`, `cast_release` —
  and the hitbox activates, the arrow leaves, and the sound plays on the frame
  the art shows it happening. This is what stops the common defect where a
  sword connects visually a hundred milliseconds after the damage landed.
- **Hitstop, and why it is presentation-only.** On a landed hit both parties
  hold for a few frames; it is the cheapest and most effective impact cue there
  is. It is applied **only** on the client, to the visual: the authoritative
  simulation never pauses, because a sim that stalls on a hit would diverge
  between two clients watching the same fight. Feel effects that would change
  the simulation are refused by that rule, not by case-by-case judgement.
- **A hit is a chord, not a number.** Hitstop, a brief flash, knockback along
  the hit normal, a particle burst, a directional sound, and a damage figure —
  layered, and each scaled by the hit's weight so a light jab and a heavy
  overhead do not read the same. Screen shake is capped and disabled under
  reduced motion (WS18).
- **Input buffering and grace windows, as stated numbers.** An action pressed
  during the previous action's recovery is buffered for a bounded window and
  fires on exit, so combos are rhythmic rather than twitchy. A dodge accepted
  slightly after a landing, and an ability accepted slightly before a cooldown
  ends, both within stated bounds. The server enforces the same bounds, so
  generosity is a rule rather than a client-side lie.
- **Cancels are a validated table.** Which action states may be cancelled into
  which, as data checked at load — so a designer tunes the combat's fluidity
  without touching code and cannot author an unreachable or infinitely
  cancellable state.
- **Enemy intent is legible.** A windup holds a readable pose and an area
  attack lays a ground decal for its shape during the windup. In a top-down
  game with ranged attackers and traps, an unreadable threat is not difficulty,
  it is unfairness, and players correctly read it as a bug.
- **It is tested, not eyeballed.** Host tests assert an action's event phases
  land at their authored frames, that the buffer and grace windows accept and
  reject at their boundaries, that no cancel edge escapes the table, and that a
  hit's presentation chord fires exactly once per landed hit. The M3 exit
  criterion requires this at a simulated 100 ms round trip, because feel that
  only exists on a loopback connection is not feel.

### Progression and the action bar (WS11)

XP curves, skill trees (a DAG validated at load — acyclic, reachable, and its
point budget consistent, or the document is refused), item affixes, and
equipment slots. The action bar is a configurable set of slots bound to items,
spells, or abilities, per character, with keyboard and pointer activation and a
binding editor; bindings resolve through `lib/keymap` so they respect the
user's layout. Slot count and layout are the player's, stored per character.

## 6. WS12 — the self-balancing economy

The goal is a currency that holds its value and prices that respond without
oscillating — and the honest way to get it is a controller with a stability
argument, not a heuristic.

- **The ledger is the truth.** Every currency faucet (quest reward, drop,
  vendor sale) and every sink (repair, tax, consumable, death penalty,
  crafting loss) is a recorded, typed transaction in the store. The realm's
  money supply is therefore a computed quantity, not a guess, and inflation is
  observable rather than inferred.
- **Vendor prices move on a damped proportional-integral controller, per item
  class.** Its input is the *median* recent trade price and the observed
  stock/demand imbalance — median, because a mean is trivially moved by one
  absurd trade. Its output is a price multiplier, clamped to a per-class band,
  rate-limited per interval, with the integral term clamped separately so it
  cannot wind up while the output is saturated at the band edge. Defaults ship
  tuned and enabled; a realm may retune the gains and the band, and may not
  remove the clamp.
  - **The stability claim is empirical, and stated as such.** The median input
    makes the loop non-linear, so a linear stability proof would not apply and
    is not offered. What is offered is a derivation of the default gains from
    the sampling interval and the rate limit, plus the §15 simulations that
    demonstrate convergence without ringing across the shock set. "Validated
    over the tested envelope" is the honest claim; "provably stable" would not
    be, and a plan that overclaims here would be worse than one that measures.
- **Arbitrage is closed by construction.** A vendor's buy price is always
  below its sell price by at least the class's spread, so buying and selling in
  a loop loses money at every scale. Per-player rate limits bound how fast one
  actor can move a class at all.
- **Sinks scale with wealth.** Repair and tax costs are a function of item
  value, so a wealthier realm drains faster — a proportional term that keeps
  the supply bounded without a wipe.
- **It is measured, not asserted.** Host tests simulate realms over long
  horizons under injected shocks (a duplication exploit, a mass sell-off, a
  population spike, a faucet left open) and assert bounded price drift,
  convergence within a stated interval, no oscillation, and no negative or
  overflowing balance. A property test drives random faucet/sink schedules and
  asserts the invariants hold for all of them. A blown bound is a defect fixed
  in the change, exactly like a failed test (§2.16).

## 7. WS1/WS8 — the realm and the wire

### Three processes

- **`wintersund`, the gateway.** Owns the listening socket and every client
  connection. Performs the handshake and authentication, enforces per-peer
  rate and bandwidth limits, relays chat, and routes each client's intents to
  the zone that owns its character. It holds `CAP_NET` and
  `CAP_NET_BIND_PRIVILEGED` only if its configured port needs it; the default
  port is unprivileged.
- **`wintersun-zone`, a shard.** Simulates one region of the realm. Holds no
  socket and no database handle: it speaks only to the gateway and the store.
  A zone crash therefore loses one region's live state, which the store's last
  committed tick restores, and cannot reach a player record or another zone.
  Zones hand characters over at their boundaries through an explicit transfer
  that is committed before it is acknowledged.
- **`wintersun-store`, the single writer.** The only process with the database
  open for writing (`plans/RECDB.md`). Serialises every commit, so the realm
  needs no distributed transaction, and is the only place a durability claim is
  made.

### The protocol

Fixed-width, versioned, little-endian frames with a bounded decode that is
total and fails closed — the `lib/abi` discipline applied to a game
(§24.4: the frame and field bounds are security bounds and stay fixed). One
fuzz harness per decoder (§19.6), and the corpus keeps every crash ever found.

Built (WS1) in `userland/games/wintersun/net`; `docs/src/userland/wintersun-net.md`
is the reference. Four settled points the rest of the game builds on:

- **The handshake is two plaintext messages plus a refusal**, and the realm
  always answers — `Hello` / `ServerHello`, or a `Refused` naming its reason,
  because a refusal before any key exists still has to say why. The encrypted
  session begins after them, and `Welcome` is its first realm message.
- **A record's nonce is its sequence in its direction and its length header is
  the associated data**, which is what refuses a reordered, replayed,
  truncated, extended or reflected record with no extra check. Any such
  failure *ends* the session and the end latches, so a peer cannot probe the
  transport one bad record at a time.
- **Every encoding is canonical.** A narrow variant is zero-padded and the
  padding is checked, an absent optional's id must be zero, and trailing bytes
  are refused — so a value has exactly one spelling and a decoded frame
  re-encodes to the bytes it came from. That equality is what the fuzz
  harnesses assert.
- **An entity on the wire is identity, kind, position, motion and facing.**
  Health, resources, equipment and status join it with the item that
  introduces them; a field with no consumer is surface nobody has reviewed.

- **Client → server:** `Hello`, `Authenticate`, `SelectCharacter`, `Intent`
  (movement, action, cast, interact, item), `Chat`, `ConsoleCommand`, `Ping`.
- **Server → client:** `Welcome` (realm seed, parameters, protocol version,
  the content digest), `AuthResult`, `Snapshot` and `Delta` (entities within
  interest, by tick), `WorldDelta` (stored changes to the generated base),
  `Event` (damage, cast, pickup, death — what the client needs to play a sound
  or an effect), `ChatMessage`, `ConsoleReply`, `Pong`, `Disconnect` with a
  stated reason (§2.24 — an abnormal end always says why).
- **Content *and the generator* are version-pinned.** `Welcome` carries the
  protocol version, a digest of the server's content documents, **and a digest
  of the world-generator and rules code versions**. The generator digest is not
  a formality: the client generates the terrain it walks on, so a client whose
  generator differs by one stage would draw ground the server does not simulate
  and desynchronise on collision — a defect that would present as "I fell
  through the floor" and be nearly impossible to diagnose from the symptom. A
  mismatch on any digest is refused at connect with the reason stated, never
  negotiated down.

### Playing at a hundred milliseconds

Server authority decides *correctness*; these four mechanisms decide whether
the game is playable over a real link. Each is a stated, tunable number, not an
emergent behaviour.

- **Interpolation delay.** Other entities are rendered at `server_time − one
  tick − jitter_margin`, where the margin adapts to measured arrival jitter
  within a bounded range. Too small and entities stutter on a late packet; too
  large and everyone is visibly in the past. Adapting it is what keeps both
  ends of that trade off the player's screen.
- **Prediction and reconciliation, for your own character only.** The client
  predicts its own movement immediately and replays unacknowledged intents over
  each authoritative state it receives. A correction below a stated threshold is
  **blended out over several frames**; above it, it snaps. Always snapping makes
  ordinary latency look like teleporting; never snapping lets a large
  divergence persist — so both paths exist with the threshold as the stated
  knob.
- **Lag compensation for aimed attacks, bounded.** For a projectile or an
  aimed shot the server rewinds candidate targets to the shooter's reported
  view time before testing — "favour the shooter", without which no ranged
  combat feels fair at distance. The rewind is **clamped to a maximum and
  validated against the shooter's own measured round trip**, so a client cannot
  claim an arbitrary rewind and shoot into the past. That clamp is the
  anti-cheat: the mechanism is generous within a bound the server computes and
  the client cannot influence.
- **Client-side effects, server-side truth.** A cast plays its animation, its
  sound and its particles at once; the damage is the server's. When the server
  refuses, the effect is retracted visibly rather than silently — a cast that
  played and did nothing reads as a bug, where one that visibly fizzles reads
  as a miss.

### Crowding: the case where everyone stands in one place

Interest management bounds bandwidth by distance, which fails exactly when a
hundred players gather in one market square — the load case every persistent
world meets on its first busy evening, and the one a distance-only scheme is
blind to.

- The per-client entity set is **hard-capped**, not merely distance-filtered.
  Above the cap, entities are chosen by priority — party and group members,
  combatants engaged with you, the nearest, then the rest — so a crowd degrades
  into "you see the ones that matter" instead of either flooding the link or
  silently dropping something you were fighting.
- Area-of-interest queries run over the zone's uniform grid, so a query costs
  the cells it overlaps rather than the zone's population; a crowd raises the
  cost of the cells it occupies and of nothing else. A naive all-pairs scan
  would be O(n²) in exactly this case and is refused.
- A zone whose population exceeds its budget **splits**, and the gateway routes
  to the split; it does not degrade the tick rate for everyone present. Tick
  rate is a correctness-adjacent property — the feel numbers above are all
  authored against it — so it is the last thing allowed to move.

### Finding a realm, and trusting the clock

- **Discovery.** A realm is reached by address, from a client-side list the
  player edits, plus optional link-local discovery for a realm on the same
  network. There is no central directory service: that would be infrastructure
  TAIRiX does not have and a privacy surface nobody asked for.
- **The server's clock is the only clock.** Every rate limit, cooldown, cast
  time and regeneration tick is measured against the *server's* monotonic
  clock, never a client-supplied timestamp. Client sample times and view times
  are inputs to be validated and clamped (above), never authority. This closes
  the whole speedhack family — a client that lies about how much time has
  passed is simply describing a window the server will not honour — and it is
  why the sub-tick sample time is safe to accept.

### Confidentiality and authentication

The session is wrapped in an authenticated encrypted channel built from
`lib/crypto`'s audited primitives — X25519 (P4) for agreement, ChaCha20-Poly1305
for the records, Ed25519 for the realm's identity, and HMAC-SHA256 over the
transcript as the key schedule — composed as a Noise-style handshake. The realm
signs the transcript rather than a fresh challenge, so its signature
authenticates *that* exchange and cannot be lifted onto another; the ephemeral
keys mean a later compromise of the identity key does not open a recorded
session. Signing stays outside the crate: `lib/crypto` exposes verification
only, so the responder takes a signer callback and the realm's secret never
enters the protocol code. Composing audited primitives is the
charter's crypto rule; inventing a primitive is not (§2.12). The realm's public
key is pinned by the client on first connect and a change is surfaced, so a
credential cannot be harvested by a substituted server.

A **local** player on the realm's own machine authenticates by the kernel's
attestation of the caller instead of a password: the gateway reads the peer's
unforgeable origin and needs no secret at all. A **remote** player has a realm
account whose authenticator is a PBKDF2 password record or a pinned public key,
following `lib/users`' record discipline (§5.1's constant-time verification,
one indistinguishable failure so accounts cannot be probed) without duplicating
its on-disk format.

### Interest management and the thousand-player floor

Each zone keeps a uniform spatial index of its entities. A client receives only
entities within its awareness radius, at a rate that falls off with distance —
near entities every tick, mid at a fraction, far as coarse position only. This
is what bounds the per-client byte rate independently of realm population, and
it is what makes a radar cheat structurally impossible rather than detected.

Everything is bounded and fails closed (§24.3, §26.2, §26.4): connections per
realm, connections per source address, bytes per second and frames per second
per peer, pending intents per peer, entities per zone, chat rate, and store
queue depth. Reaching a bound refuses the request with a reason and records it;
it never allocates without limit. Back-pressure propagates: a client that
cannot keep up is sent coarser deltas and then disconnected with a reason, and
never allowed to grow an unbounded queue in the gateway.

Nothing spins (§2.23). The gateway, the zones, and the store all park on a
`waitset` over their sockets, IPC endpoints, and one-shot timers, and are woken
by the event.

## 8. WS7 — persistence

The engine is `plans/RECDB.md` (P5); the realm's schemas are the game's:

- **Account** — identity, authenticator, created/last-seen `Time64`, role,
  moderation state.
- **Character** — owner, name, archetype, level and XP, stats, the figure
  parameters the designer produced, position and zone, resources.
- **Inventory and equipment** — stacks, affixes, durability, bound slot.
- **Progression** — skill allocations, discovered map regions, quest state.
- **World delta** — sparse, keyed by chunk: terrain and material edits,
  structures, depleted or respawning resource nodes, container contents.
- **Economy** — the transaction ledger and the per-class controller state.
- **Realm** — seed, parameters, content digest, the last committed tick.
- **Audit** — moderation actions and administrative commands, which also go to
  the system log's hash-chained trail (§19.4), because a realm administrator
  must not be able to erase their own record.

**The tick is the *consistency* unit; it is emphatically not the commit
cadence.** State is only ever captured at a tick boundary, so a transaction can
never hold a half-applied action. But committing every tick would mean a
durability barrier twenty or thirty times a second *per zone* — a hundred-plus
`fsync`-equivalents a second across a realm, which no disk survives and which
would make the store the whole realm's bottleneck. The two concerns are
separated:

- **Durability-critical events commit synchronously, before acknowledgement.**
  A logout, a completed trade, a level-up, an item created or destroyed, a
  currency movement, a zone handover. These are the things a player must never
  lose, and each is rare, so paying a barrier for each is affordable. "I logged
  out and lost my loot" stays structurally impossible.
- **Routine state commits on a bounded cadence**, whichever of these comes
  first: a dirty-record count, a dirty age, or a flush interval — all derived
  from the device's measured characteristics rather than hand-picked (§24.1).
  Position, resource regeneration and cooldowns are in this class. A crash
  therefore rewinds a character's *position* by up to the interval and nothing
  more, which is the standard and acceptable trade every persistent world
  makes.

This is the same distinction `plans/ARXFS-WRITEBACK.md` draws between a dirty
set and an explicit `fs_sync`, applied a layer up; the barrier is paid where
durability is actually claimed, not per tick.

## 9. WS14 — sound

`plans/SOUND.md` owns the stack (P1, P2). The game's design decision is how it consumes
it: **the game mixes its own voices into a single `audio-v1` stream.** `audiod`
mixes application streams to a device; a game mixing sixty-four footsteps,
spell tails, and thunder claps is one application composing its own stream,
exactly as it composes one window surface from many controls. It is not a
second system mixer, and it must not re-implement one: the conversion,
resampling, and channel-map arithmetic come from `lib/audio`, and only voice
management — emitter position to gain and pan, priority and voice stealing,
ducking, reverb zones, distance filtering — is the game's.

- Positional audio is 2D: gain from distance with a documented falloff, pan
  from bearing relative to the camera, low-pass with distance so a far sound is
  dull rather than merely quiet.
- **Thunder is delayed by the speed of sound from the strike, and rolls.**
  Lightning flashes, and the clap arrives a real interval later, low-passed by
  distance. It is one line of arithmetic and it is the single most convincing
  detail in a storm.
- Weather beds are continuous loops whose gain tracks precipitation intensity
  and whose character changes with wind; footstep and impact sounds are chosen
  by the material under the foot, which the splat field already knows.
- Music is a small adaptive set, cross-faded by region and combat state.
- Every compressed asset decodes in the §19.5 sandbox through `lib/sound`; the
  game holds decoded PCM, never a decoder in its own address space.
- A realm with no audio device, or a session that does not hold the seat's
  sink lease, plays no sound and continues (§2.24 — a refused optional action
  is reported, not fatal).

## 10. WS5/WS16/WS18 — the client's surfaces

### Windowed, maximised, and exclusive fullscreen

Three size states over P3's window channel. Windowed and maximised are
ordinary. Fullscreen asks the compositor for a scanout-sized surface promoted
to a single layer — no composition pass, a tear-free flip, and a resolution
change through the existing `DISPLAY_ENDPOINT` `Configure` where the player
chose one. Losing the seat (a fast user switch) is an event, not a crash: the
game pauses the simulation clock it owns, releases the sink lease, and resumes
at the exact position when the seat returns (`plans/DISPLAY.md`).

All UI lengths are authored in logical pixels and converted through
`tairix_geometry::Scale`, so the game is correct at any DPI and UI scale (`AGENTS.md` §10).
Every control is a `lib/controls` control with its specified states, theme
variants, and keyboard path (`plans/GUI-CONTROLS-DESIGN.md`) — the game
hand-rolls no widget.

### The console

An overlay command surface: history, completion, and a command set whose help
comes from the bundle's own `Help/` tree through `lib/help`, never a hardcoded
string (§16.5). A client command affects only the client (graphics, audio,
diagnostics). A realm command is sent as `ConsoleCommand` and authorised
**server-side** against the account's role — a client-side role check is
decoration, and the server never trusts one.

### Chat

Channels: say (local radius), party, guild, whisper, and realm. Every message
is length-bounded, stripped of control characters, rate-limited per account,
and rendered as data — never interpreted, never a command path, never able to
inject a control sequence into a terminal or a console. Per-account mute and
block lists are honoured server-side so a blocked message is never sent, not
merely hidden.

### Admin and moderation (WS16, WS15)

Roles are realm records. A moderator can mute, kick, and report; an
administrator can ban, adjust the economy's parameters within their clamps,
inspect entities, and shut the realm down cleanly. Every action is recorded in
the audit schema **and** the hash-chained system log (§19.4). `wintersunctl` is
a `kind = command` bundle in the system command store, so administration is
typeable, scriptable, and documented like any other command; it speaks the
gateway's control endpoint and follows the GNU-coreutils option and output
conventions the charter requires of a command app (§16.7), and it emits
`stdinfo` advisory records on fd 3 alongside its ordinary output (§20.1).

### The character designer (WS17)

The engine is `plans/FIGURE.md` FG6/FG7; the surfaces are the game's. A
category rail (species, build, face, hair, markings, palette, gear) over
`lib/controls` sliders and pickers, with a **live preview that plays an
animation** and a clip selector — a build judged in a static pose is a build
whose walk nobody checked — and a simultaneous small-size preview, so the
silhouette readability `plans/FIGURE.md` §4 measures is visible while authoring
rather than discovered by a failing test. Presets, a randomise-plausible
button, and the character library, stored through the store process.

Two obligations bind it, and it is the surface most likely to breach both
(§28): a slider changes the parameter in memory and repaints — it opens no
store and writes nothing per motion sample — and the durable write happens once
when the drag settles; and a repaint rebuilds only what the changed parameter
invalidates, so a palette edit re-tints and does not re-rig. On submission the
**server re-validates every parameter** against its bounds, because the record
arrives from a client and an impossible figure must be refused rather than
drawn (`plans/FIGURE.md` FG6).

### Accessibility and localisation (WS18)

Not a late pass — the charter already binds most of it. Reduced motion honours
the desktop's setting and damps camera shake, flashes, and particle density.
Lightning flash intensity is capped and separately adjustable, because an
uncapped white flash is a photosensitivity hazard. Colourblind-safe variants of
every status and faction colour, with shape and pattern carrying the same
information (§15 of the controls spec). Full input remapping, keyboard-only
play, and pointer-only play. Subtitles and captions for audio cues, which a
player with no audio device needs anyway. UI scale independent of window size.
All strings and the help tree are per-locale with the deterministic fallback to
`en-US` that `lib/help` already defines.

### The detail-level control (WS18)

§3 states the mechanism: `auto` is the default, it sheds in a fixed order to
hold the frame, and it will not cross the readability floor. This is the
surface over it.

**What WS5 already built, so this item does not re-plan it:** the ladder
itself (`quality::Ladder` — the step space, the five rungs and their notches)
and the governor that drives it from measured frame time
(`budget::Governor`, with a run of overruns to shed and a longer run of
comfortable frames to restore, the two thresholds far enough apart that they
cannot chase each other). What remains is the **mode**, the **floor**, and the
**surface**.

**Two modes, and `auto` is the default.** A new install adapts; a player who
wants a fixed picture says so. The mode and the chosen level are one setting,
because "auto" and "level 3" are answers to the same question and holding them
apart invites a stored level nobody is using.

**The presets are the ladder's own rung boundaries, not a second table.**
`Full`, then one stop per rung fully shed — so adding a rung adds a stop and
the two cannot drift. The slider detents are exactly those stops: there is no
free-running detail number, because a value between two rungs draws the same
picture as one of them and would only produce settings files that cannot be
compared. On `auto` the slider is disabled and reads back the live level, so a
player can see what the machine settled on before deciding to pin it.

**Choosing a level below the floor is allowed and is labelled.** The surface
states what the choice costs — telegraphs and silhouettes stop being
guaranteed readable — and then honours it. Preventing the choice would be
deciding for a player who may be running on hardware this plan never
anticipated; hiding the cost would be worse.

**§28 binds this control, and it is the charter's own worked example.**
Dragging the slider changes the level in memory and repaints; it opens no
store, sends no request, and writes nothing. The durable write happens once,
when the drag settles, and the repaint is scoped to what the level actually
changed rather than re-deriving the frame. Switching mode writes once. The
setting is the client's own per-app data (`plans/APPDATA.md`), never sent to
the realm and never an input to the simulation or the digest.

Verification: the floor is derived from FG5's checks rather than stated as a
number, and a test drives the governor to the floor and asserts it stops
there; a forced level survives a frame-time storm unchanged; the preset stops
equal the rung boundaries by construction, asserted rather than listed; and a
simulated drag produces exactly one durable write and one repaint per drained
input burst.

## 11. Resource limits and the operating-conditions floor

The game and the realm are held to §24 and §26 like any other subsystem.

- No capacity is a hand-picked constant (§24.1). Chunk cache, entity budgets,
  connection counts, and the store's page cache are all derived from the
  discovered machine and grow on demand, failing closed only on genuine
  exhaustion as a typed error (§4, §2.9).
- **The stated floor:** a realm serving a thousand players and a client
  rendering at a playable rate must both hold on a modest machine, and the
  conjunction is what is tested, not each in isolation (§26.7). The client's
  resident set is its working set — the chunks and materials on screen — not
  the world's extent. The realm's is its live entities and its page cache, not
  its player count on disk.
- Under memory pressure everything reclaimable shrinks through `lib/reclaim`'s
  bands before anything refuses: material mips, chunk caches, decoded artwork,
  audio beds, and the store's page cache, in that order (§26.3).
- A failing disk under the store is an expected outcome, surfaced as a typed
  error and an audited event, never a panic and never silently served data the
  store cannot vouch for (§26.5).

## 12. Diagnosing it, and iterating on it

Two things decide a team's velocity on a project this size, and neither is a
feature a player sees.

- **A desync must be bisectable, not guessable.** The simulation is
  deterministic, so every tick has a state hash. A client and server exchange
  hashes periodically; on a mismatch the client reports the **first diverging
  tick** and both sides dump the per-entity hashes for it, so the answer is
  "entity 412's velocity diverged at tick 90 113" rather than "players
  disagree". Without this, one non-deterministic line costs days to find; with
  it, minutes. The intent log plus the seed replays the whole session, which is
  also the moderation audit trail.
- **A frame's cost is attributable.** The per-pass budget (§3) is *measured* at
  runtime, not just in tests: the client records per-pass timings, the
  active degradation step, its mode, and whether it has reached the
  readability floor, readable from the console. A budget nobody can
  observe in the running game is one that silently rots.
- **Content reloads without a restart.** Spells, items, skill trees, loot
  tables and biome parameters are declarative documents, and the server
  re-reads and re-validates them on an admin command, rejecting an invalid set
  **without** dropping the live one. Tuning a spell's windup must cost seconds,
  not a rebuild and a relog — iteration time is the single largest multiplier
  on how good the combat ends up being. A reload is refused for anything that
  would invalidate live state (a removed item a player holds), with the reason
  named.

## 13. Risks, and what would be done about them

A plan this size without a risk register is a plan that has not been thought
about. Each entry names the trigger, the mitigation, and — where one exists —
the criterion for abandoning the approach rather than sinking more into it.

| Risk | Severity | Mitigation and kill criterion |
|---|---|---|
| **The software renderer misses the frame budget** at 1280×720 on the reference machine | High | The stated degradation order and render scaling absorb an overrun down to the readability floor (§3); below it the frame rate gives way and the diagnostic says so, rather than the picture quietly becoming unreadable. Measured at M1, which exists for this. If 720p60 is unreachable after the SIMD and tiling work, the baseline drops to 960×540 and is **stated** rather than quietly missed; the renderer is not rescued by cutting the visual design. |
| **Cross-target determinism breaks** | High | `lib/util::mathf` is FMA-free and intrinsic-free today, which is what makes the claim affordable; the M1 four-target hash vertical is the gate, and a change introducing `mul_add` into an authoritative path is a defect. Escape hatch if it proves unholdable: fixed-point arithmetic for the authoritative sim — costly, so it is a fallback, not a plan. |
| **The audio stack (P1) slips** | Medium | WS14 sits late deliberately, so M1–M3 do not block on it. The game ships silent and says so; it does not grow a private audio path (§14). |
| **The `cinder` migration regresses a shipped feature** | Medium | `cinder`'s existing shape, paint, gait and roam tests plus its QEMU vertical are the acceptance gate. If its pixels cannot be preserved, that is surfaced (§15.7), not absorbed. |
| **The thousand-player target is unmet** | Medium | Interest management, the per-client cap and zone splitting are the levers, and each degrades gracefully: the realm serves fewer players per zone rather than failing. The number is a measured property (§15), so a shortfall is reported with the figure reached. |
| **Scope** — the whole body of work is multi-year | High | The milestone structure exists for this: every milestone exits with a playable or measured artefact, so the work is steerable and cancellable at each boundary rather than all-or-nothing. |
| **The economy is gamed in a way the shock set did not model** | Low | The ledger makes exploitation observable after the fact, the clamps bound the damage while it is happening, and the admin surface can retune within them. A new exploit becomes a new shock case in the test set. |

## 14. Refused by name

Stating these once stops each being re-proposed.

- **A scripting VM for content** (Lua or otherwise) — untrusted code execution,
  a JIT surface, and a C dependency. Content is data over a closed vocabulary
  (decision 5).
- **A private GPU path, or a second renderer.** The game reaches a GPU through
  `plans/GPU.md`'s seam like every other consumer, and the software renderer
  stays complete and mandatory without it. OpenGL specifically is refused there,
  on the grounds that no GPU is tied to it and its object model aged badly —
  not on the withdrawn argument that a C-specified API cannot be implemented in
  Rust.
- **A client-authoritative anything.** Including "trusted" clients, host
  migration, and client-side hit detection.
- **A second renderer, rasteriser, or blend path** for the game, and a private
  framebuffer or GPU back-channel that bypasses the compositor (§2.2, §17.3).
- **A second system audio mixer.** The game composes one stream (§9).
- **A game-specific kernel capability or syscall.** Realm roles are records; the
  game's authority is an ordinary app's (decision 7).
- **Anti-cheat by client inspection** — scanning a player's memory or
  processes. Authority is the answer; surveillance is not, and TAIRiX will not
  ship a process that reads another's memory for a game.
- **Tile-grid terrain**, and shipped photographic terrain textures. Materials
  are synthesised and splatted (§3).
- **A `/proc`-style stats file or a fabricated virtual filesystem** for realm
  telemetry. Stats come from the control endpoint and `stdinfo` (§16.6, §20.1).
- **Real-money transactions, loot boxes, or any wagering mechanic.** Out of
  scope by design, not merely unimplemented.

## 15. Verification

Every item lands with its tests; these are the claims the plan is judged on.

- **Determinism.** A fixed seed and a fixed intent log produce one state hash
  after N ticks, identical on `x86_64`, `aarch64`, `riscv64` and `wasm32`
  (decision 4). Run as a QEMU vertical per target.
- **World.** Chunk generation is pure and halo-bounded: a chunk generated alone
  equals the same chunk generated as part of its neighbourhood. Rivers flow
  downhill everywhere. Roads connect the sites they claim to. No biome weight
  vector is unnormalised. Generation is reproducible after an interruption at
  any stage.
- **Rules.** Hand-computed damage, healing, resistance, and status cases; XP
  and level boundaries; skill-tree documents validated and malformed ones
  refused; a proptest that no sequence of legal actions produces a negative or
  overflowing stat, resource, or balance.
- **Economy.** Long-horizon simulations under injected shocks assert bounded
  drift, convergence, no oscillation, and closed arbitrage (§6).
- **Protocol.** Round-trip and bounded-decode tests for every frame; fuzz
  harnesses for every decoder with the regression corpus; a test that an
  oversize, truncated, or reordered frame is refused and the connection ended
  with a stated reason.
- **Game feel.** Each action's event phases land at their authored frames; the
  input buffer and grace windows accept and reject exactly at their bounds; no
  cancel edge escapes the validated table; a landed hit fires its presentation
  chord exactly once; and hitstop never advances or stalls the authoritative
  tick. Run at a simulated 100 ms round trip, which is the M3 exit criterion —
  feel that exists only on loopback is not feel.
- **Netcode under latency.** Over injected latency, jitter and loss: a
  reconciliation below the threshold is blended and one above it snaps (both
  asserted at the boundary); the interpolation margin adapts within its range
  and does not oscillate; a lag-compensation rewind beyond the clamp, or
  inconsistent with the shooter's measured round trip, is **refused**; a
  refused cast is visibly retracted rather than silently dropped.
- **Crowding.** With many times the per-client cap of players in one grid cell:
  the per-client entity set respects the cap, priority selection keeps party
  members and active combatants in the set, per-client bandwidth stays bounded,
  the area-of-interest query cost scales with overlapped cells and not with
  zone population, and a zone over budget splits without the tick rate moving.
- **The clock cannot be lied to.** A client reporting inflated elapsed time,
  back-dated sample times, or replayed intents gains no cooldown, cast-time,
  movement or regeneration advantage — one test per channel.
- **Diagnosis works.** An injected non-determinism is caught by the periodic
  state-hash exchange and reported as the *first* diverging tick with
  per-entity hashes, and the intent log plus seed replays the session to that
  tick. A content reload with an invalid document is refused with the live set
  intact.
- **The frame budget is measured, per pass**, at the baseline resolution on the
  reference machine, and the degradation order is exercised: each step engages
  in the stated sequence under injected overload and frame rate is the last
  thing to move.
- **Security.** A client claiming an impossible move, an unaffordable cast, an
  item it does not hold, a role it lacks, or an entity outside its interest is
  refused and audited — one test per claim. Chat with control characters is
  sanitised. A substituted realm key is surfaced.
- **Multiplayer vertical.** Two guests over the QEMU network path (`cargo
  xtask netpeer` is the existing precedent): connect, authenticate, both see
  each other move, one casts and the other takes damage, one disconnects
  uncleanly and the realm survives with the store's last tick intact.
- **Client vertical.** The game launches, opens a window, renders a
  deterministic frame from a fixed seed, and the composited pixels are read
  back and hashed. The three size states transition and the fullscreen surface
  is promoted to a single layer. A seat switch pauses and resumes exactly.
- **§28 compliance.** No frame-loop store read, file read, or IPC round trip; a
  slider drag produces one write; a pointer-motion burst produces one frame; a
  repaint's damage is scoped to what changed. Asserted, not asserted-about.
- **Art quality** is gated by `plans/FIGURE.md`'s contact-sheet goldens and
  readability checks, because "the art is good" is otherwise unfalsifiable.
- **Floor.** A realm at its stated population with multiple zones on a modest
  discovered-RAM configuration: bounded resident set, growth then fail-closed
  on exhaustion, no panic, no busy-spin (§26.7).
- **Oracles.** `loom` models for the client↔server frame ring and the store's
  submission queue, since both are lock-free producer/consumer protocols whose
  correctness is an ordering claim; `miri` enrolment for any crate carrying
  `unsafe` — which, on present design, is none of the game's own, because the
  only `unsafe` in the render path is `lib/parallel`'s already-enrolled
  `for_each` (§19.11).

## 16. Open decisions

Each is a real question this plan does not pretend to have answered. Whoever
reaches one stops and asks (§15.7) rather than choosing silently.

1. **An unreliable channel for position deltas.** TCP is correct and complete
   for the session, the world deltas, and events, and the protocol is framed so
   an additional unreliable channel for near-entity positions would be an added
   channel rather than a redesign. Whether it is worth the second path at this
   scale is unmeasured, and the decision waits on a measurement (§2.16).
2. **Zone shards across machines.** The gateway/zone split is already a process
   boundary, so distributing zones over a network is a transport change rather
   than an architectural one. Not in scope; the question is whether the
   zone↔gateway protocol should be designed for it from the start.
3. **Whether `cinder` belongs in `userland/games/` too.** It is a virtual pet
   with needs, intents and roaming — game-shaped by any reading — but it is
   also the first holder of `CAP_DESKTOP_LAYER` and the desktop-layer seam's
   only proof, and `plans/CINDER.md` CD13 wants a *second* holder to show the
   seam is a seam. Moving a mostly-finished feature to make a taxonomy tidy is
   not obviously worth it, and this plan does not need it: after FG1 the two
   share `lib/raster`'s outline primitives and nothing else. Recorded because
   the question will be asked, not because an answer is pending.
