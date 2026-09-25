# PLAN.md — TAIRiX Build Plan

The ordered stages that build TAIRiX and every workstream around them. A
workstream with its own plan under `plans/` owns its design, invariants and
per-item ledger; this file records its overall status and does not repeat the
plan. The topic → plan jump-sheet is `AGENTS.md` §15.18.

- **Status lives in the ledger** below and nowhere else in this file.
- **Done means green.** An item is done only when the validation gate is green
  over the whole workspace (`AGENTS.md` §2.15, §7); a stage starts only once its
  dependencies are done.
- **A task brief** names the ledger ids it covers, confirms their dependencies,
  and points at the governing plan; the charter's agent rules (§15) and review
  gate (§23) apply without restatement.

## Ledger

A workstream with its own plan has one row here; its per-item rows live in that
plan's ledger. A `blocked` row names its blocker.

### Stages

| Id | Item | Status |
|---|---|---|
| S0 | Stage 0 — repository foundation: workspace, toolchain pin, lint and deny policy, `cargo xtask`, CI | done |
| S1 | Stage 1 — the shared `no_std` libraries every later stage imports | done |
| S2 | Stage 2 — the architecture-neutral kernel core, sub-stages 2.1–2.8 | done |
| S2.7f | Stage 2.7 follow-up — the production syscall path, f1–f6 | done |
| S3 | Stage 3 — the four Tier-1 ports 3a–3d and the Arch HAL | done |
| S4 | Stage 4 — driver framework and first drivers | in progress |
| S4.HW | Stage 4.HW — hardware detection and driver autoload | done |
| S5 | Stage 5 — the VFS and the ARXFS, ext4, FAT32 and ADFS drivers | done |
| S6 | Stage 6 — userland foundations: init, shell, login, command apps, System Information API | done |
| S7 | Stage 7 — graphics, window manager, taskbar and default apps | in progress |
| S7-UA | Stage 7 user-memory copy path, increments A–E | done |
| S8 | Stage 8 — installer and image builders | in progress |
| S9 | Stage 9 — security hardening and audit | planned |
| S10 | Stage 10 — release engineering | planned |

### Items this file owns

| Id | Item | Status |
|---|---|---|
| P-1…P-7 | The preemptive kernel and its blocking primitives | done |
| L1–L5 | §24 resource limits and derived capacities | done |
| H1 | §19 item 1 — the hash-chained journal core and `journald` | done |
| H2 | §19 item 2 — signed log anchors and per-service `CAP_LOG_WRITE` | planned |
| H3–H9, H11, H13 | §19 items 3–9, 11 and 13 | done |
| H10 | §19 item 10 — KPTI and branch-predictor barriers, stack canaries and the shadow stack | planned |
| H10-REPRO | §19 item 10's supply-chain half — bit-reproducible images and no-post-install-fetch enforcement | planned |
| H12 | §19 item 12 — Gold (Verus) contracts and the CHERI Tier-2 port | planned |
| H13-MTE | §19 item 13's remainder — enable Arm MTE on `FEAT_MTE` silicon | planned |
| LOG | The rest of the system log: the `log` command bundle, `journald` installed and launched with its vertical, boot-ring import, retention, one machine identity | planned |
| T64 | §20/§21 — `stdinfo` and 64-bit-native time | done |
| TSC | Untrusted-timer resolution: the invariant-TSC check and `CAP_TIME_HIRES` | done |
| SH-P4 | Shell prerequisite P4 — `lib/path` and the volume forest; open: runtime-volume alias policy, an `fs::` root | in progress |
| SH-P5 | Shell prerequisite P5 — `lib/resref` and its resolvers; open: the kernel-owned device namespaces (`plans/ALIAS.md`) | in progress |
| SH-P6 | Shell prerequisite P6 — `lib/glob` and `lib/complete` | done |
| BUNDLES | Self-contained bundles, increments 1–4 | done |
| BUNDLES-5 | Self-contained bundles increment 5 — delete the kernel-baked spawn registry | blocked: x86_64 and riscv64 need their storage floor (`plans/ARCHSUPPORT.md` A1–A2) |
| BOOTCONF | Boot facts and the boot-time configuration store | done |
| TASKID | Task ids drawn per boot rather than counted | done |
| LLC | Cache-aware (LLC-aware) scheduling | planned |
| FS-FD | An open descriptor holds its resolved node and grant, not a path re-resolved on every call | planned |
| FS-WALK | One driver descent per path component instead of `lookup`, `node_info` and `security` | planned |
| FS-MAC | Measure the per-block MAC on Pi hardware; adopt a faster audited authenticator if it dominates | planned |
| FMAP-X | Wild-jump cases in the aarch64 and riscv64 file-map verticals (x86_64 has `wild_fault_qemu_x86_64`) | planned |
| ENT-RNG200 | Pi 4 platform entropy from a discovered, health-tested BCM2711 RNG200 source | planned |
| FONT-PICK | A desktop font-family choice, persisted per user, validated against `FontRequest::Families` | planned |
| README-SHOTS | Recapture the README gallery's text-console images (old 68×27 grid) and add the Settings Wallpaper pane | planned |
| UNLOCK-FLAKE | A correct passphrase refused once under parallel `ci` load | planned |
| CI-PAR | Overlap the cross-compiled program builds by target triple (about 90 s of `ci`) | planned |
| PI-METAL | On-metal Raspberry Pi 4 acceptance of the work QEMU cannot model | blocked: needs an operator run on a Pi 4 |
| DOC-REFS | Stale comments and docs found while rewriting this file | planned |
| PLAN-SYNC | Sub-plans whose own status contradicts their body or the tree | planned |

### Kernel, memory and platform

| Id | Item | Status |
|---|---|---|
| WIRING | Arch HAL migration and cross-arch parity, W0–W17 (`plans/WIRING.md`) | done |
| ARCHSUPPORT | x86_64 product parity with aarch64 (`plans/ARCHSUPPORT.md`) | in progress |
| FINISH-x86_64 | An x86_64 image booting from UEFI firmware to the desktop in QEMU; the Surface Go 2 next (`plans/FINISH-x86_64.md`) | planned |
| PI | Raspberry Pi 4 bring-up (`plans/PI.md`) | in progress |
| BOOTLOADER | The first-party Rust UEFI/BIOS boot chain and GPT image (`plans/BOOTLOADER.md`) | in progress |
| NEW-SUPERVISOR | The pre-boot Supervisor console (`plans/NEW-SUPERVISOR.md`) | in progress |
| SPAWN | Process spawn and userland multitasking (`plans/SPAWN.md`) | done |
| THREADS | Threads within a process and the futex (`plans/THREADS.md`) | done |
| FIX-SLEEPLOCK | Wait-queue registration identity and the sleeping lock's handoff (`plans/FIX-SLEEPLOCK.md`) | done |
| FIX-SYSCALL | Syscall bodies with interrupts enabled (`plans/FIX-SYSCALL.md`) | in progress |
| FIX-KHEAP | Fragmentation-immune kernel-heap growth (`plans/FIX-KHEAP.md`) | done |
| SMARTRAM | Reclaimable-memory caches over one pressure gauge (`plans/SMARTRAM.md`) | done |
| SWAPSWAPSWAP | `ramzip`, the encrypted compressed RAM tier (`plans/SWAPSWAPSWAP.md`) | done |
| FIX-SWAPFILE | Encrypted partition swap below `ramzip` (`plans/FIX-SWAPFILE.md`) | planned |
| WATCHDOG | The CPU soft- and hard-lockup watchdog (`plans/WATCHDOG.md`) | in progress |
| FIX-PANICS | The kernel-panic register dump and backtrace; on-target symbolication staged (`plans/FIX-PANICS.md`) | done |
| FIX-WILD | Debuggable user-fault kills and the crash record (`plans/FIX-WILD.md`) | done |
| FIX-STALLTRACE | Stack-traced interactive frame overruns (`plans/FIX-STALLTRACE.md`) | done |
| FIX-RANDOMNESS | The RNG tier split and the fast-key-erasure generator (`plans/FIX-RANDOMNESS.md`) | done |
| FIX-PROTECTION | Stack canaries, the shadow stack, MTE, the protection-fault fix-up (`plans/FIX-PROTECTION.md`) | planned |
| FIX-HARDWARE-FEATURES | CPU feature detection and `lib/cpuops` routine selection (`plans/FIX-HARDWARE-FEATURES.md`) | in progress |
| CPUFREQ | CPU frequency scaling on the Raspberry Pi (`plans/CPUFREQ.md`) | done |
| COLLECTIONS | The shared container and hashing libraries (`plans/COLLECTIONS.md`) | in progress |
| RECDB | `lib/recdb`, the durable B+tree record store (`plans/RECDB.md`) | planned |
| TPM | TPM support and measured boot (`plans/TPM.md`) | planned |
| UNIVERSAL | Multi-arch bundles and a Wasm app tier (`plans/UNIVERSAL.md`) | planned |

### Storage and filesystems

| Id | Item | Status |
|---|---|---|
| IMPLEMENT-OUTSTANDING-ARXFS | The ordered ledger of every outstanding ARXFS item (`plans/IMPLEMENT-OUTSTANDING-ARXFS.md`) | in progress |
| ARXFS-METADATA | Extended attributes and foreign-filesystem metadata (`plans/ARXFS-METADATA.md`) | in progress |
| ARXFS-WRITEBACK | The dirty set, run coalescer, commit scheduler and barrier (`plans/ARXFS-WRITEBACK.md`) | done |
| ARXFS-MAINTENANCE | Autonomous scrub, discard and health (`plans/ARXFS-MAINTENANCE.md`) | in progress |
| ARXFS-SNAPSHOT | Read-only snapshots, diff and send/receive (`plans/ARXFS-SNAPSHOT.md`) | planned |
| ARXFS-FEC | Forward error correction and multi-device pools (`plans/ARXFS-FEC.md`) | planned |
| SPARSE | ARXFS sparse files (`plans/SPARSE.md`) | done |
| SYMLINKS | Symbolic and hard links, and canonicalisation (`plans/SYMLINKS.md`) | done |
| FILELOCK | Advisory byte-range file locks (`plans/FILELOCK.md`) | in progress |
| DRIVES | The storage-namespace design brief (`plans/DRIVES.md`) | done |
| ALIAS | Resource references, aliases and resolvers (`plans/ALIAS.md`) | in progress |
| NEW-NAMESPACE | Boot namespace assembly from signed policy (`plans/NEW-NAMESPACE.md`) | planned |
| FIX-IO | Storage I/O fault isolation (`plans/FIX-IO.md`) | in progress |
| DEVICES | `lspci`/`lsusb`, USB mass storage, hotplug automount (`plans/DEVICES.md`) | done |

### Drivers, devices and graphics hardware

| Id | Item | Status |
|---|---|---|
| fixdrivers | Device logic lives in `drivers/`, not `lib/*` (`plans/fixdrivers.md`) | done |
| USB | The modular USB stack and hot-removal (`plans/USB.md`) | done |
| SOUND | The audio stack and the `dmaengine-v1` DMA-engine seam (`plans/SOUND.md`) | in progress |
| FIX-DISPLAY-ACCELERATION | Hardware layer composition and `gpu_virtio` (`plans/FIX-DISPLAY-ACCELERATION.md`) | planned |
| GPU | The `lib/gpu` render and compute seam and its backends (`plans/GPU.md`) | planned |
| SHADER | SPIR-V, its validator, and the WGSL front end (`plans/SHADER.md`) | planned |

### Networking and remote access

| Id | Item | Status |
|---|---|---|
| NETWORK | The dual-stack network stack (`plans/NETWORK.md`) | done |
| DHCP | The DHCPv4 and DHCPv6 clients (`plans/DHCP.md`) | done |
| DNS | The DNS stub resolver (`plans/DNS.md`) | in progress |
| ZEROCONF | mDNS / DNS-SD discovery: `discoveryd`, the `lib/discovery` client and `dns-sd` (`plans/ZEROCONF.md`) | in progress |
| TELNET | The `telnet` client (`plans/TELNET.md`) | blocked: T2 needs a TLS stream transport over `lib/crypto` |
| SSH | The SSH client, server and tools (`plans/SSH.md`) | in progress |

### Userland, services and applications

| Id | Item | Status |
|---|---|---|
| APPS | Bundles, help, command resolution, the coreutils set (`plans/APPS.md`) | in progress |
| APPDATA | Per-app settings, secrets, blobs and scratch (`plans/APPDATA.md`) | done |
| CAPABILITY_USE | The capability lifecycle from login to administration (`plans/CAPABILITY_USE.md`) | in progress |
| USERS | System and service accounts (`plans/USERS.md`) | done |
| NEW-SERVICEMANAGER | The service manager (`plans/NEW-SERVICEMANAGER.md`) | in progress |
| NOTICE | System notices (`plans/NOTICE.md`) | in progress |
| SYSLOG | The system log specification; its build state is H1, H2 and LOG (`plans/SYSLOG.md`) | in progress |
| TIMESYNC | The NTP client, RTC drivers and clock provenance (`plans/TIMESYNC.md`) | in progress |
| TIMEZONES | Civil time zones (`plans/TIMEZONES.md`) | planned |
| SHELL | The `elsh` shell specification (`plans/SHELL.md`) | in progress |
| CURSES | `lib/vt`, `lib/termcap` and `lib/curses` (`plans/CURSES.md`) | in progress |
| PTY | Pseudo-terminals and the shared line discipline (`plans/PTY.md`) | done |
| IO | The userland I/O library (`plans/IO.md`) | in progress |
| CCOMPAT | The C-callable ABI: headers, stubs, crt0 (`plans/CCOMPAT.md`) | done |
| VIM | The `vim` command app (`plans/VIM.md`) | in progress |
| STRESSTEST | `sysmon`, `stress` and the observability they need (`plans/STRESSTEST.md`) | in progress |
| VIEW | The picture and document viewer (`plans/VIEW.md`) | in progress |

### Desktop

| Id | Item | Status |
|---|---|---|
| DISPLAY | Seats, the display lease and the graphical session (`plans/DISPLAY.md`) | in progress |
| NEW-DESKTOP-LOGIN | The greeter, the session authority and fast user switching (`plans/NEW-DESKTOP-LOGIN.md`) | in progress |
| APPWIN | Default apps on live channels, and the file picker (`plans/APPWIN.md`) | done |
| COMPOSITOR-WORK | Server-side window decorations (`plans/COMPOSITOR-WORK.md`) | done |
| GUI-CONTROLS-DESIGN | The Reactive Alloy control set (`plans/GUI-CONTROLS-DESIGN.md`) | done |
| NEW-MENUS | Session-owned menus (`plans/NEW-MENUS.md`) | done |
| TOOLTIPS | Seat-owned tooltips (`plans/TOOLTIPS.md`) | in progress |
| NEW-TASKBAR | The icon bar (`plans/NEW-TASKBAR.md`) | done |
| NEW-SWITCHBOARD | The Switchboard window (`plans/NEW-SWITCHBOARD.md`) | in progress |
| NEW-DESKTOP-SETTINGS | The Settings application (`plans/NEW-DESKTOP-SETTINGS.md`) | done |
| NEW-FILEMANAGER | The graphical file manager (`plans/NEW-FILEMANAGER.md`) | in progress |
| GUI-TERMINAL | `terminal.app` (`plans/GUI-TERMINAL.md`) | in progress |
| PINBOARD | The wallpaper, the Desktop folder and the backdrop menu (`plans/PINBOARD.md`) | blocked: P9's picked-directory listing needs an ABI decision on `FilePicked` |
| ICONS | Icon artwork tiers and the sandboxed decode cache (`plans/ICONS.md`) | in progress |
| SVG | The `lib/svg` decoder (`plans/SVG.md`) | in progress |
| FONT-SERVICE | `fontd` and the `/System/Fonts` store (`plans/FONT-SERVICE.md`) | in progress |
| FIX-DESKTOP | Non-blocking launch and I/O off the interactive loop (`plans/FIX-DESKTOP.md`) | in progress |
| FIX-DESKTOP-SPEEDUP | Software redraw speed (`plans/FIX-DESKTOP-SPEEDUP.md`) | in progress |
| CINDER | The desktop companion and `CAP_DESKTOP_LAYER` (`plans/CINDER.md`) | in progress |

### Games

| Id | Item | Status |
|---|---|---|
| WINTERSUN | The WinterSun RPG (`plans/WINTERSUN.md`) | in progress |
| FIGURE | Figure rigs, meshes and motion (`plans/FIGURE.md`) | done |

### Engineering

| Id | Item | Status |
|---|---|---|
| OPEN-DEFECTS | The core-kernel defect tracker (`plans/OPEN-DEFECTS.md`) | in progress |
| CODEVERIFY | The recurring code-quality sweeps (`plans/CODEVERIFY.md`) | in progress |
| WAFFLE | The comment-discipline sweep (`plans/WAFFLE.md`) | planned |

## Stage 0 — Repository foundation

The workspace builds every Tier-1 target from a clean clone under the pinned
toolchain (`rust-toolchain.toml`: `nightly-2026-07-03` with `rust-src`,
`llvm-tools-preview`, `clippy`, `rustfmt` and the four cross targets; the pin
needs cargo-deny 0.19 or later). `.cargo/config.toml` carries the `xtask` alias
and the per-target flags; `rustfmt.toml`, `clippy.toml` and `deny.toml` are
enforced by `cargo xtask ci`. All pipeline logic lives in `tools/xtask`;
`tools/ci/` holds only thin wrappers (`ci-run.sh`, `soak.sh`) for an unattended
builder. `.github/workflows/ci.yml` runs `cargo xtask ci` on every push, and
`soak.yml` runs the nightly soaks. Fuzz, proptest and filesystem-soak seeds come
from `tests/fuzzseed`: fresh per run, pinned for replay with
`TAIRIX_{FUZZ,PROPTEST,FSSOAK}_SEED`. `LICENSE` is GPL-2.0-or-later with the
`TAIRiX-syscall-note` ABI exception.

## Stage 1 — Shared libraries (`lib/`)

The `no_std` foundation every later stage imports: `lib/abi` (the `abi-v1`
types, fuzzed; not frozen until the first release), `lib/caps` (subset-only
delegation, exhaustively property-tested), `lib/crypto` (audited upstream
crates behind first-party wrappers, `zeroize` on), `lib/log`, `lib/rng` (the
HMAC-SHA256 DRBG, the ChaCha12 fast-key-erasure generator, the xoshiro256++
non-cryptographic generator, and the entropy and hardware-RNG seams), the
allocation-free `lib/inline` beside the heap-backed `lib/collections`,
`lib/memguard` (the one guard-region sentinel and canary window, shared by the
slab guard, the kthread stack guard and every port's boot-stack guard), and
`lib/util`.

`lib/util` admits only an item with two or more independent callers, and its
README requires this file to name them:

| Module | Callers |
|---|---|
| `argv` | `useradd`, `usermod`, `groupadd`, `passwd`, `mount` |
| `cfloat`, `cnum` | `seq`, `printf` |
| `conf` | `lib/sysconfig`, `lib/netconfig`, `userland/system/init` |
| `count`, `tailwindow` | `head`, `tail` |
| `defer` | the terminal's and the desktop session's settings publishers, the session's catalogue scan, the file manager's bundle scan and occupancy probes |
| `fallible` | `lib/raster`, `userland/gui/wm`, `lib/image` |
| `fmt` | `kernel/sec`, `kernel/ipc` |
| `mathf` | `lib/fontface`, `lib/svg` |
| `retry` | `userland/system/timed`, `userland/system/init` |
| `secret` | `lib/rt`'s elevation client, `elsh`'s `elevate`, `login`'s elevation broker, `lib/controls`' masked field |
| `size` | `du`, `df` |

## Stage 2 — Kernel core (architecture-neutral)

The sub-stage numbers are cited by the crates they delivered.

- **2.1 `lib/sync`**: spinlock, IRQ-safe spinlock, writer-preference `RwLock`,
  MCS queue lock, `SeqLock`, `Once`/`OnceCell`. It needs only `core`, so a
  freestanding binary that links it supplies no allocator. Choosing a lock:
  `docs/src/architecture/sync.md`.
- **2.2 `kernel/mem`**: the buddy/bitmap `FrameAllocator`, per-process
  `AddressSpace<P: PageTable>` over the Arch HAL, the guard-page `Slab`,
  zero-on-free sensitive allocations, a `Result` from every allocation, and the
  boot RAM sanity test (`ramtest.rs`: an address-line and stuck-bit check that
  fails closed with the failing location, not a march test).
- **2.3 `kernel/sched`**: the `SchedulerPolicy` contract in `kernel/sched/api`
  with its conformance suite, and the sibling policies `cfq` (the default),
  `eevdf` and `mlfq`.
- **2.4 `kernel/sec`**: the identity table, per-task capabilities (user grant ∩
  manifest request), fail-closed Ed25519 manifest verification, and the audit
  writer. `uid 0` holds no power.
- **2.5 `kernel/ipc`**: capability-checked ports (checked at bind and on every
  send, so a receiver never re-checks), shared-memory objects whose revocation
  invalidates every mapping, and notifications.
- **2.6 `kernel/core`**: `kernel_main`, `BootInfo` and the `KernelArch` trait,
  and a panic path that logs and halts, never silently resets. No global
  mutable statics.
- **2.7 `kernel/syscall`**: the dispatch table generated from
  `lib/abi/src/syscalls.rs`, its SHA-256 pin re-checked at boot and by
  `cargo xtask abi-check`.
- **2.8 QEMU harness**: the `tools/qemu` runner (direct `-kernel` boot,
  `isa-debug-exit`, a strict wall-clock budget, no retries) behind
  `cargo xtask test --qemu`.

## Stage 2.7 follow-up — Production syscall wiring

The production syscall path runs end to end: the per-CPU current-task slot
(f1), the task-to-capability registry (f2), the production `SyscallHandlers`
(f3), the `DispatchCallbackSlot` and its `Phase::Syscall` hook (f4), the
production dispatcher installed by `kernel/tairix-kernel` (f5), and the
`syscall_dispatch_qemu` vertical (f6). The payload copies it deferred landed
with the Stage 7 user-memory copy path.

## Stage 3 — Architecture ports and the Arch HAL

All four ports implement the whole Arch HAL and pass its conformance suite under
their native QEMU target: 3a `x86_64` (multiboot2/UEFI hand-off, ACPI MADT,
LAPIC and IO-APIC, INIT-SIPI-SIPI), 3b `aarch64` (QEMU `virt` with GICv2, the
generic timer and PSCI; the Raspberry Pi 4 is `plans/PI.md`), 3c `riscv64`
(QEMU `virt` with PLIC, CLINT and SBI), and 3d `wasm32` (browser workers over
`MessageChannel`, isolated by linear memory).

The closed HAL slice set is enumerated in `AGENTS.md` §17.2, and its per-slice
state lives in `plans/WIRING.md`. The one slice authorised but not yet built is
`shadowstack` (`plans/FIX-PROTECTION.md` P4). The CPU-feature slice and the
`lib/cpuops` dispatch framework built on it are
`plans/FIX-HARDWARE-FEATURES.md`; their shared vocabulary lives in
`tairix_abi::cpufeatures`, so the framework takes no `kernel/*` edge.

## Stage 4 — Driver framework and first drivers

The per-class driver traits live in `lib/abi/src/driver/`. The driver host
(`userland/system/drvhost`) admits a driver only through its load gate:
`CAP_DRV_LOAD` (and `CAP_DRV_KERNEL` for an in-kernel driver), a validated bind
table, and a signature covering the header, capability list, bind table and
payload. The first drivers are `display/{vesa,framebuffer}`,
`input/{ps2,usb_kbd,usb_mouse,virtio_kbd}`, `storage/{virtio_blk,emmc2}`,
`network/virtio_net`, and `accelerator/virtio_crypto` (AES-CBC, with a session
created and destroyed per job so no key outlives its call, and a published
per-job ceiling a larger job is refused against). The bus drivers
(`bus/mmio`, `bus/virtio`, `bus/pcie_brcm`, `bus/usb/{xhci,vl805}`, with the
PCI configuration mechanisms in `lib/pci`) reach a register window only through
the window the host maps for them, never by mapping one themselves, and every
MMIO and DMA grant is capability-checked and audited. Every emulable driver has
a load, use, unload and reload QEMU vertical, and the Stage 4.D acceptance gate
(a green `ci` with coverage at the charter's thresholds) is met.

**Remaining.** `ps2` still polls; it should park on its interrupt line.
`lib/virtio::PackedQueue` is host-tested, but no transport negotiates
`VIRTIO_F_RING_PACKED`, so packed virtqueues have no consumer yet.

## Stage 4.HW — Hardware Detection and Driver Autoload

`AGENTS.md` §18, delivered:

- **One inventory.** `lib/abi/src/hwtree.rs` is the architecture-neutral
  hardware tree. Each port's discovery normalises its native source (FDT, ACPI,
  the host query) into it, and the kernel's live `HwTreeStore` carries the
  generation counter that `hw_tree_wait` parks on.
- **One match policy.** A driver declares its bind table (`DriverBindKey`: an
  `HwMatchKey` with class wildcards and a priority, at most
  `DRIVER_MANIFEST_MAX_BIND_KEYS`) in its signed manifest. Every match, in the
  kernel floor or in user space, goes through `lib/devmatch`: the highest
  priority wins, an unbroken tie is a packaging defect, and an unmatched node is
  left unbound and logged.
- **Policy in user space, mechanism in the kernel.** `devmgr` reads the tree
  (`hw_tree_read`, under `CAP_SYSINFO_HW`), parks on `hw_tree_wait`, and asks the
  kernel-resident driver store over `DRIVER_STORE_ENDPOINT`
  (`lib/abi/src/driver_store.rs`) for its catalogue (opaque bundle ids and the
  bind tables the kernel decoded) and to load or unload a bundle for a node. No
  bundle byte or path crosses to user space; the kernel re-reads the bundle,
  re-runs the signed gate, and spawns it.
- **A driver gets exactly its node's resources.** They arrive as unforgeable,
  owner-checked grants (`resource_grants`, `mmio_map`, `dma_alloc`,
  `shm_create_dma`, `msi_alloc`). A bus driver publishes the children it
  enumerates with `hw_emit_node`/`hw_remove_node` under `CAP_HW_EMIT`, and only
  within windows it already holds (`HwResource::covers`). A driver that dies
  while its device may still master memory surrenders its DMA carves, and the
  DMA shared regions it still maps, to a quarantine held against its node, which
  frees them, scrubbed, only on a later driver's reset-confirmed `dma_quiesced`
  or the node's retirement. A node has at most one live driver and its id is
  never reissued, a live driver withholds what its device may still own, and a
  node's removal revokes its authority from its driver and every delegate,
  tearing down their windows, interrupt bindings and granted regions before it
  returns (`plans/OPEN-DEFECTS.md` D167, D225–D227, D230).
- **The floor is the storage path.** `driver_catalog::IN_KERNEL_DRIVERS` holds
  only virtio-blk, plus EMMC2 on aarch64. The signed driver store lives on a
  read-only `/System` volume mounted before the encrypted root is unlocked and
  kept mounted for life, so even the unlock keyboard is an autoloaded user-space
  driver (`plans/PI.md`, designs B and D).
- **The Pi 4 USB keyboard** is five autoloaded pieces — FDT discovery,
  `pcie_brcm`, `vcmailbox`, `vl805`, `usb_kbd` — ordered by the nodes they
  publish, so the keyboard's node exists only once `vl805` has reloaded the
  controller firmware; completion is MSI-driven. Its live run is PI-METAL.

The delivery items cited elsewhere were: 1, the drvhost spawn seam; 2, the bind
table; 3, `devmgr`; 4, generic match-key emission; and 5, the Pi chain moved
onto autoload, delivered as design D (D1–D5).

## Stage 5 — Filesystem

The arch-neutral VFS in `kernel/core/src/fs/` resolves absolute paths only. It
enforces the per-inode model (mode, ACL and capability gate) through one
fail-closed `Metadata::authorize` that never branches on `uid == 0`, and mounts
`/System` read-only with `Logs` and `Settings` writable. The OS authors only the
four root-view names; the VFS reserves no error for a legacy name, and a
top-level create is ordinary write permission on `/`. Drivers: `arxfs`
(native), `ext4` (read and write, with `metadata_csum` validated against
`mke2fs`/`e2fsck`), `fat32`, and `adfs` (every Acorn FileCore format, with RISC
OS metadata carried by `lib/fsmeta`'s `acorn.*` keys). Each ships a first-party
`format` and fails with `NoSpace` on exhaustion. Coverage: the `posix_fs_suite`
(pjdfstest-equivalent), `fs_soak` (`cargo xtask fssoak`, every formatter over a
1 GiB `RamBlock`), and the ARXFS-over-virtio-blk vertical.

**ARXFS** is one on-disk version with one mandatory profile (every feature on,
none tunable): copy-on-write, always encrypted, checksummed, compressed and
deduplicated, SSD-aware, recoverable, with sparse files and 255-byte names. The
spec is `docs/src/filesystem/arxfs-spec.md`; every outstanding ARXFS item, with
its owner and order, is `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`.

**Path-walk cost.** Filesystems run uncached on first access by design, so a
redundant read costs a device round trip plus a whole-block MAC. Listings resume
in O(1) from a `getdents`-style cursor, a directory entry carries its child's
`NodeInfo` (so `du` needs one open and one listing per directory), and repeat
access is served by the rebuildable `CachedFs` (`plans/SMARTRAM.md`). FS-FD,
FS-WALK and FS-MAC are what remains.

**Demand-paged file mappings.** `file_map`/`file_unmap` (76/77) give a
read-only, never-executable, demand-paged private mapping out of the per-task
file window. Each page is resolved on first access through the secured VFS
under the mapping-time identity. Every other user fault, data or instruction, on
every bare-metal port, goes to the one kill policy (`terminate_user_fault`, audited as
`TaskFaultKilled`), which ends the task with 139 and never the machine. Design:
`docs/src/architecture/memory.md` §7f/§7o.

## Stage 6 — Userland foundations

PID 1 `init` (the dependency-ordered service manager, reaper and manifest
capability granting), `elsh`, and `login`; the core command apps, each its own
bundle; the System Information API (`lib/abi/src/sysinfo.rs`, whose
`SYSINFO_QUERIES` registry names each query's capability) served by
`userland/system/sysinfod`; the signed app-bundle loader (granted = user ∩
manifest, fixed `.app` layout) with the dynamic loader restricted to the
bundle's `Libraries/` and `/System/Libraries/`; and the `/System/Security/Users`
database (`lib/users`: PBKDF2 records, a fuzzed fail-closed parse,
timing-equalised authentication).

`login` runs the desktop when `os.loginType` is `graphical` (the default) and a
graphical session is available, and the account's shell otherwise. It waits
without prompting while the encrypted root is still being unlocked (the
three-state `users_db_read`), so it never draws over the unlock prompt. One
login is supervised per installed text console, and local echo is the kernel's
line discipline. A keyboard driver injects device-resolved key edges with
`key_inject` under `CAP_INPUT_INJECT`, and the kernel encodes and routes each by
whoever holds the seat: the focused console's queue, or the desktop's keyboard
channel. The root volume is unlocked by an in-kernel kthread that
reads the FAT `root.unlock` descriptor, derives the key, mounts ARXFS and loads
the users database; it tries a blank passphrase silently first, so the
installer image unlocks with no prompt.

## Stage 7 — Graphics, window manager, taskbar

**Deliverables.** The compositing `userland/gui/wm` (per-window surfaces,
damage, anti-aliased rounded corners with a square opt-out, per-surface and
per-region premultiplied alpha, and the one rounded-corner path the taskbar also
draws through); the icon bar (`plans/NEW-TASKBAR.md`); dark and light themes,
switchable at runtime, driving the WM, taskbar and apps from one `lib/theme`
definition; a themed cursor set; SVG-first assets with the three-tier raster,
vector, built-in-glyph resolution (`plans/ICONS.md`); and the default apps, a
file manager (`plans/NEW-FILEMANAGER.md`) and a terminal
(`plans/GUI-TERMINAL.md`). Headless compositor, taskbar-layout, theme-switch and
input-routing tests cover each. Docs: `docs/src/desktop/`.

**What stands.** The desktop is the `desktop.app` bundle
(`userland/gui/session`) in the system application store, spawned by `login` or
typed at a shell. It composites in user space and presents to the display
service over `display_ipc` (`plans/DISPLAY.md` D7), holding the seat lease the
kernel checks on every present. Windows are served over `WINDOW_ENDPOINT` with
server-side decorations, and input arrives seat-routed (`pointer_read` and
`keyboard_read` under the live lease), never over a named port. Each shared
desktop library holds one path: `lib/raster` (the only rasterise, blend and
resample path), `lib/theme`, `lib/geometry` (the one logical-to-physical
`Scale`), `lib/reclaim`, `lib/font` (a thin client of the sandboxed `fontd`,
`plans/FONT-SERVICE.md`), `lib/fontface`, `lib/cursor`, `lib/icon`, `lib/svg`,
`lib/input`, and `lib/controls` (`plans/GUI-CONTROLS-DESIGN.md`).

**User-memory copy path**, cited by the crates it delivered: A,
`kernel/mem::uaccess` `copy_in`/`copy_out`; B, the per-task
`AddressSpaceRegistry`; C, `with_caller_aspace`, which threads it into the
syscall handlers without a `kernel/syscall` → `kernel/mem` edge; D.1–D.4, the
payload copies for `ipc`, `cap_delegate` and `random_get`; E, the per-port
fault-window fix-up (`tairix_arch_api::uaccess`), proven by
`uaccess_fault_qemu_*`.

**Platform entropy.** `tairix_arch_api::entropy` seeds the kernel reserve at
boot from `RDSEED`/`RDRAND` or `RNDR`, XOR-mixed with a timing-jitter source
and the interrupt-arrival pool so no single source is trusted alone. The Pi 4's
Cortex-A72 has no `FEAT_RNG`, so it boots unseeded and `random_get` answers
`EntropyNotReady` — honest, never weakened — until ENT-RNG200 lands.

## Stage 8 — Installer and image builders

**Deliverables.**

- `userland/system/installer`, today a placeholder crate: text and graphical
  front ends over one core, implementing `AGENTS.md` §11 and laying out §16 —
  the four roots, the §11 and §16.3 mount flags, reserved legacy names refused
  in expert mode, and encrypted root and encrypted swap by default, with
  plaintext swap never offered.
- Swap brought up only through `kernel/mem::swap::EncryptedSwap`, the sole user
  of a `SwapBackend` (ChaCha20-Poly1305 per page, an ephemeral per-boot key,
  fail closed). The envelope exists; the pager, the backend and the activation
  syscall are `plans/FIX-SWAPFILE.md`.
- `tools/mkimage` images: `images/tairix-x86_64.iso` over the first-party boot
  chain (`plans/BOOTLOADER.md`, whose GPT/ESP builder `plans/ARCHSUPPORT.md` A1
  needs), `images/tairix-riscv64.img`, and `images/tairix-web/`. The Raspberry
  Pi image is built (`cargo xtask image --target aarch64-rpi [--profile
  debug|installer]`, `plans/PI.md` P9): an MBR, a FAT partition of pinned and
  checksummed firmware, the read-only signed `/System` volume and the encrypted
  ARXFS root, each laid down by the in-tree drivers. `cargo xtask run --target
  aarch64-rpi` boots the same image windowed on QEMU `virt`.

**Tests.** An end-to-end QEMU install (build, boot, install, reboot, log in as
the created user, check permissions and layout), and a headless browser test of
the `wasm32` image. **Docs.**
`docs/src/install/{x86_64,raspberry_pi,riscv64,web}.md`.

## Stage 9 — Security hardening and audit

The §19 burn-down supersedes this stage wherever the two differ. What it adds is
a threat-model document (`docs/src/security/threat_model.md`), a review of every
driver still in the kernel with a move to user space wherever possible,
penetration scripts under `tests/security/`, and
`docs/src/security/{hardening,reporting}.md` beside the existing `audit_log.md`.

## Stage 10 — Release engineering

A versioning policy (semver on the ABI, distinct from the product version), a
release checklist (`docs/src/release.md`), signed releases of all four images,
and the upgrade-path documentation that begins with the first frozen `abi-v1`.

## Preemption and blocking (P-series)

The kernel is preemptive on every bare-metal port and never runs a cooperative
dispatch loop. The cited ids:

- **P-1** — a timer interrupt taken from user mode involuntarily reschedules
  the interrupted task through the context-switch path a syscall trap uses; a
  tick taken in the kernel never preempts it (`preempt_el0_qemu_*`).
- **P-2** — the blocking wait queue: a waiter parks off the run queue and is
  woken by its event or, with a deadline, by the scheduler's one-shot timer.
- **P-3** — `init` launches the perpetual `devmgr`, which parks on
  `hw_tree_wait` rather than consuming scheduler turns.
- **P-4** — tickless preemption: the timer is armed one-shot, for one quantum,
  only while a CPU has a ready competitor (`SchedulerArch::set_preemption`). CFQ
  is the sanctioned periodic exception (`AGENTS.md` §17.1).
- **P-5** — the dispatch loop runs in-kernel tasks with device interrupts
  deliverable, masking only around the idle park. Interrupt-context wakes are
  lock-free (`WaitQueue::request_wake`) and drained at a dispatcher-context
  point (`waitq::drain_pending_wakes`), so no scheduler lock is ever taken with
  interrupts off. `preempt_inkernel_qemu_aarch64` proves it.
- **P-5b** — syscall bodies run with device interrupts enabled, and a latched
  tick or an unparked task yields at the return to user
  (`plans/FIX-SYSCALL.md`).
- **P-5c** — an in-kernel body gives the CPU up at a safe boundary
  (`preempt::yield_if_owed`) at most one operation after its quantum expires.
- **P-6** — the wait queue meets the foundational bar: an O(log n) three-index
  `WaitSet` (membership, FIFO arrival order, deadlines) with a stated
  first-come-first-served discipline and wake-one paths
  (`kernel/core/src/waitq.rs`).
- **P-7** — every other foundational primitive (the `lib/sync` locks,
  `OnceCell`, `lib/inline::BitSet256`, `lib/caps`, `kernel/ipc`, the
  allocators) was audited against the same bar and passes. One watch-item:
  `kernel/mem::Slab::alloc` scans `in_use` in O(slot count). Its only caller
  uses one slot, and the first large-slot consumer must add an O(1) free index
  in its own change.

## §17 Modularity Enforcement and Burn-down

`cargo xtask deps-check` (layering, the one-way GUI edge, no concrete scheduler
named outside `kernel/sched/*` and `kernel/core`) and `cargo xtask cfg-check`
(target-conditional code only inside the allow-list) run in `ci` with empty
grandfather lists, and `cargo xtask build --headless` proves the headless image
(`docs/src/architecture/modularity.md`). Page-table teardown extended two
existing HAL slices in place rather than adding one:
`PageTableFrames::free_table` with the shared `frames::reclaim_hierarchy` walk,
and `AddressSpace::reclaim_table_frames`. Each CPU parks on its boot
translation at every task suspend, which is what makes the teardown SMP-safe
(`docs/src/architecture/memory.md`).

## §19 Threat Model and Hardening Burn-down

Item numbers are the `H` rows of the ledger; code cites them as "§19 burn-down
item N". The burn-down keeps §17's shrink-only, fail-closed discipline. Items 2
and 10 carry **[DO IMMEDIATELY ON UNBLOCK]**: once its blocker clears, the item
is done before any other stage work. Both blockers have cleared — H2 waited on
an on-device signing API and a persisted log store, and H10 and H13-MTE on the
user/kernel boundary — so those three are next in line
(`plans/FIX-PROTECTION.md`, `plans/ARCHSUPPORT.md` A8).

- **H1** — the hash-chained journal: `lib/log`'s chain, segment, record,
  dictionary, authority, boot-ring, ingress and journal engine; `journald`,
  which writes immutable `/System/Logs/<stream>/<id>.seg` segments; the
  machine-id as the chain genesis; and the renderers and the `log` tool
  library. What remains of the log is LOG.
- **H2** — signed log anchors (the root hash signed at least once a minute and
  at clean shutdown, persisted under `/System/Logs/Anchors/`) and per-service
  `CAP_LOG_WRITE`. Segments are sealed with `LogAttestationKey` today. A debug
  image bakes its key; the production key is the one the Stage 8 installer mints.
- **H3** — `cargo xtask sbom` (deterministic CycloneDX; unsigned until the
  per-installation key exists). **H4** — `cargo xtask supply-chain`: the source-hash allow-list and
  the advisory SLA (`supply-chain.toml`). **H5** — `cargo xtask fuzz` over every
  harness. **H6** — the Bronze proptest models and `cargo xtask spec-review`.
  **H7** — the `rxe` loader (R/RX/RW only, PIE, CFI tag) and `EnterUser`.
  **H8** — honest side-channel profiles on every port and `lib/crypto`'s
  constant-time test under `-O3`. **H11** — the Silver TLA+ model and
  `cargo xtask model-check`. **H13** — memory tagging: the `MemoryTagging` HAL
  and the on-by-default software use-after-free check in the slab.
- **H9** — parser sandboxing: `SPAWN_FLAG_SANDBOX` admits a capability-empty,
  syscall-fenced child, and `lib/sandbox` is the one request/reply and session
  seam; the `plans/APPS.md` S8 sweep moved every userland parser of foreign data
  behind it. Kernel-side parsers of platform data stay fail-closed, bounded and
  fuzzed, and are isolated by the user-space driver model instead.
- **H10** — the hardening the user/kernel boundary gated: KPTI and
  IBPB/IBRS/STIBP/SSBD on x86_64, KPTI and the Spectre-v2 branch sequence on
  aarch64, and stack canaries and the shadow stack (`plans/FIX-PROTECTION.md`).
  **H10-REPRO** is its supply-chain half: bit-reproducible images
  (`cargo xtask build --reproducible`) and no-post-install-fetch enforcement.
- **H12** — Gold (Verus contracts on `lib/caps` and the `kernel/sec`
  capability check) and the CHERI Tier-2 port. Aspirational per the charter.

## §20 / §21 — `stdinfo` and 64-bit time

`Time64`/`Duration64` (`lib/abi/src/time.rs`) are the only absolute-time ABI
and on-disk types, and narrowing one is checked (`TimestampOutOfRange`, never a
wrap). `stdinfo` is fd 3, with the closed `StdInfoRecord` kinds and an
allocation-free JSONL writer. ARXFS stores its four timestamps as true
`Time64`, and every driver reports them in `NodeInfo::times` from the same read
that yields the node.

## Untrusted timer resolution (§19.1)

x86_64 refuses to bring up a second CPU without an invariant TSC, and measures
the TSC's frequency against the PIT rather than trusting CPUID. `clock_get`
returns raw nanoseconds only to `CAP_TIME_HIRES` holders; everyone else gets the
`COARSE_CLOCK_GRANULARITY_NS` (1 µs) floor through the one `coarsen_clock_ns`.

## §24 Resource Limits and Scalability

Every capacity is derived from discovered hardware or grows on demand, and the
settable limits follow the `ulimit` model
(`docs/src/architecture/resource-limits.md`). Security and format bounds on
untrusted input stay fixed (§24.4), as does the §22 RNG reserve.

- **L1** — the rlimit ABI: `lib/abi/src/rlimit.rs`, `rlimit_get`/`rlimit_set`,
  and `CAP_RLIMIT_RAISE`.
- **L2** — enforcement: a per-task `LimitSet` in the `AddressSpaceRegistry`,
  inherited by intersection and never widened.
- **L3a** — the derived kthread stack size (32 KiB release, 64 KiB debug) and
  a stack tier sized from discovered RAM inside the kernel remap window.
- **L3b** — the fixed-capacity sweep: the userland heap's span table, the
  supplementary-group ceiling, spawn fan-out (page tables drawn from the live
  frame allocator), every port's per-CPU bookkeeping and secondary bring-up bound
  (caller-sized storage, no `MAX_CPUS`), guarded kthread stacks drawn and
  returned per thread, the ARXFS paged allocation map (a read-only mount holds
  no allocator state), and a kernel heap that grows and shrinks, with a reserve
  that user commits cannot draw below.
- **L4a** — `ulimit` in `elsh`. **L4b** — the `RESOURCE_LIMITS` sysinfo query.
- **L5** — demand-grown user stacks inside a reserved span bounded by the
  `StackBytes` limit, on every MMU port (`plans/SPAWN.md` SP11).

## Shell prerequisites (`plans/SHELL.md`)

The shell stays a pure interpreter because the parsers it needs are shared
libraries. `plans/SHELL.md` specifies the shell itself; these are the
prerequisites it depends on, of which only P4–P6 are recorded.

- **P4** — `lib/path`, the one path-spelling parser (`/path`, `Alias:/path`,
  `alias::Name/path`, `id::<volume-id>/path`, relative), with machine-alias and
  durable `id::` resolution at the single kernel entry point and runtime volume
  attach under `/Storage/<name>` (`plans/DEVICES.md` D3).
- **P5** — `lib/resref`, the one resource-reference parser, with
  `resource_open` serving `sys:random` and `sys:null` in the kernel, and
  `lib/procinfo::resolve` serving `info:` and `stats:` in userland.
- **P6** — `lib/glob`, the one bounded, backtracking-free glob matcher, and
  `lib/complete`, the one completion policy.

## Self-contained bundles

Every discovered bundle — command apps and services alike — ships complete on
the read-only `/System` volume, its signed `AppInfo` and `Run` beside its
`Help/` tree, and `spawn` loads it through the shared `lib/appload` gate. The
binding increments:

1. The canonical bundle content hash, `tairix_abi::digest_bundle_contents`: one
   framing shared by the composer and every `BundleStore`.
2. A per-crate `AppInfo.toml`, found by a discovery walk (never a list) and
   composed and signed under the system app-signing seed, a trust domain
   separate from driver signing. The signature covers the header and the
   capability body.
3. The `image_apps` pipeline plants each bundle into `/System/Commands`,
   `/System/Applications` or `/System/Services` by its manifest kind, in the Pi
   image and in every encrypted-root QEMU fixture.
4. Disk-backed `spawn`: the kernel resolves `…/<Name>.app/Run`, reads it under
   the caller's identity through the secured VFS, verifies it against the
   embedded app trust anchor, and caches the accepted image once per boot for
   the immutable store bundles (never for writable `/Apps`). The system
   principal resolves to the capability-less bootstrap identity when no
   `uid 0` record exists, so PID 1 can start the boot services before the root
   is unlocked.
5. Delete the kernel-baked registry (`SPAWN_PROGRAMS`, `spawn_paths.rs`,
   `program_manifests.rs`, and every `*_rxe.rs` include except PID 1's) once
   x86_64 and riscv64 have their storage floor. Until then it is those ports'
   justified boot floor.

## Boot facts and the configuration store

`boot_facts_get` (89) serves one immutable `BootFacts` record — the arch, the
boot CPU's model name, the CPU count and the installed RAM — from which PID 1
renders its banner. `/System/Settings/Configuration/system.conf` is the one
administrator-settable boot-time store, with a closed key registry in
`lib/sysconfig` and the `configure` command app; a new setting is a new `Key`
and its consumer in one change.

## Task ids are drawn, not counted

One per-boot-seeded generator in `kernel/sched/api` and one id rule,
`choose_task_id`, shared by every policy: reserved ids are never drawn, a live id
is never issued twice, and a thread-group leader's id is held while its group
lives. `SchedulerPolicy::spawn_parked_as` admits PID 1 at its reserved id. Pids
are `i64` across the ABI, every state keyed by a task id is torn down before the
id is released, and the process instance `ProcId` is what tells a stale
reference from a new occupant (`fd_grant` names its recipient by it).

## Cache-aware scheduling (LLC-aware task aggregation)

Co-locate a process's data-sharing threads on one last-level-cache domain. The
binding decisions:

- It is a `SchedulerPolicy` behaviour exercised by the shared conformance
  suite; no crate outside `kernel/sched/*` and `kernel/core` learns of it.
- LLC topology is discovered into the hardware tree (ACPI PPTT and CPUID cache
  leaves, device-tree `cache` nodes), never compiled in, and is absent on
  `wasm32`.
- Aggregation runs on the load balancer behind a working-set-versus-LLC guard.
  Cache-aware balancing may default on and cache-aware wakeup defaults off, each
  decided by measurement.
- Placement is a hint: it never weakens isolation, fairness or the
  no-starvation bound, and absent topology falls back to today's placement.

Deliverables: the `hwtree` topology nodes and per-port discovery, the policy
hook with conformance cases on at least four cores and two LLCs, the EEVDF
implementation, a `sysinfo` view behind the hardware query, a multi-LLC QEMU
vertical, and the benchmark that sets each default. The `README.md` matrix row
stays planned until a port lands it.

## Open items

**LOG.** The `log` command bundle (`show`, `tail`, `find`, `boot`, `expire`)
over the host-tested `userland/shell/log` library; `journald` given its bundle
manifest, installed, launched by `init`, and proven by a QEMU vertical; the
kernel boot ring and its drain syscall, with the per-CPU gap detection that
lands with that producer; retention; and one machine identity shared by the
kernel's `SystemIdentity` and `/System/Security/MachineId`.

**UNLOCK-FLAKE.** Once, under parallel `ci` load, an aarch64 root-unlock
vertical refused the correct passphrase after a scripted wrong attempt,
re-prompted, and timed out. The suspected mechanism does not exist:
`read_passphrase_line` has no deadline, and the only timed pauses are the 3 s
wrong-attempt delay and the 2 s ESC window, so the candidate is how that delay
treats typed-ahead input. The admission vertical has since stopped typing a
wrong attempt, which removes the trigger without a diagnosis or a fix. Remaining:
reproduce it, fix it without weakening the anti-brute-force delay, and restore
the wrong-attempt step with a slowly-delivered-line regression test.

**CI-PAR.** `cargo xtask ci` takes about 15 minutes warm on 24 cores, and its 319
`pie_build` spawns (about 193 s) run serially. The private target directory is
per triple, so builds for different triples can overlap, but the grouping has to
be by triple across every `pie_build` caller (the image pipeline, the kernel
build script and the QEMU fixtures), and the change needs proof that the ELFs it
produces are byte-identical. Measured and ruled out: concurrent cargo runs on one
target directory serialise on its lock, and merging `target_clippy`'s strata
would unify features and link `lib/rt`'s allocator into the kernel. The QEMU
budget (`nproc/3`) is deliberate headroom, not a lever. Even at its floor the
pipeline outlives one agent tool call; `docs/src/contributing.md` is the
procedure for that.

**PI-METAL.** QEMU models no Pi 4 PCIe, USB, MSI, EMMC2 or VideoCore, so these
wait on an operator run with a UART log: the autoloaded USB keyboard chain and a
keystroke, MSI delivery across a post-activity idle, and serial output flowing
through the passphrase prompt (Stage 4.HW, P-5); a driver restart that recovers
its predecessor's quarantined DMA for `vcmailbox`, the VL805 and GENET
(`plans/OPEN-DEFECTS.md` D167); the EMMC2 DMA path and the SD
write-throughput figure (`plans/PI.md` P8, `plans/ARXFS-WRITEBACK.md` §8); the
GENET NIC and its offloads (`plans/NETWORK.md`, `plans/PI.md` P12); the DHCP
lease (`plans/DHCP.md` D5); USB attach, detach and mass storage
(`plans/USB.md`, `plans/DEVICES.md`); the RTC tiers (`plans/TIMESYNC.md` TS-4);
and the watchdog's aarch64 delivery (`plans/WATCHDOG.md`). `plans/PI.md` is
where each is checked off.

**DOC-REFS.** Stale comments and docs, most of them pointers into this file;
each is a line to fix in a gated change:

- "`PLAN.md` lines 154–158": `tools/xtask/src/commands/qemu_tests.rs`,
  `docs/src/platform/x86_64.md`.
- The long-gone "Stage 3a sub-checklist": `kernel/arch/x86_64/{Cargo.toml,
  src/lib.rs}`, `kernel/tairix-kernel/README.md`,
  `tests/integration/scheduler_stress{,_qemu}`, `docs/src/platform/x86_64.md`,
  `docs/src/architecture/kernel.md`.
- "Stage 4.D Item 0-tail": `userland/system/drvhost/tests/host_integration.rs`.
- Blockers that have cleared, still named as blocking: the `Pending` profile
  reasons in `kernel/arch/{x86_64,aarch64}/src/sidechannel.rs` and
  `kernel/arch/aarch64/src/memtag.rs` (the Stage 6 boundary, H10 and
  H13-MTE), and `lib/log/src/chain.rs` (the signing authority, H2).
- A rustdoc link to the deleted `driver_autoload`:
  `kernel/tairix-kernel/src/test_support.rs`.
- Claims no longer true: `lib/abi/src/syscall.rs` (the dispatch table "not yet
  introduced"), `docs/src/architecture/syscalls.md` (the Stage 2.7 deferrals
  "remaining"), `docs/src/drivers/hardware-detection.md` (hotplug removal
  "remaining"), `docs/src/security/proptest_models.md` (Silver "not yet
  scheduled"), `drivers/display/gpu_virtio/src/lib.rs` ("delivered by
  Stage 4"; it is `plans/FIX-DISPLAY-ACCELERATION.md`), and
  `drivers/filesystem/arxfs/README.md` (the removed `FilesystemTimestamps`
  trait).
- In the charter and plans: `AGENTS.md` §17.1's "see `PLAN.md` (immediate
  work)" (P-5 is done), and `plans/SPAWN.md`'s "`plans/SHELL.md` P2"
  (SHELL.md numbers nothing).
- Left by the x86_64 fault-path rework: `tests/SECURITY.md` names the deleted
  `kernel/arch/x86_64/src/idt.rs` three times; the IDT is `interrupts.rs`'s
  `Idt`, built by `percpu.rs`'s `fatal_table`.
- `docs/src/architecture/memory.md` has two §7o sections (demand-paged file
  mappings, and cold-page identification), so every `§7o` citation — Stage 5
  above, `plans/PI.md`, and five within the page — names either.
- Found reviewing the DMA-quarantine change: `docs/src/architecture/syscalls.md`
  (`port_write`'s return type, `port_bind` absent from the table, gated
  syscalls missing from the capability matrix).

**PLAN-SYNC.** Plans whose own status text disagrees with their body, their
ledger or the tree. The ledger above records what the evidence supports; each
plan's own text is corrected when it is next touched, or sooner.

- A stale status line over a finished body: FIX-WILD, NEW-SUPERVISOR (§11),
  SMARTRAM, IMPLEMENT-OUTSTANDING-ARXFS, SPAWN (SP10 and SP11 headings), PTY
  (PTY5's D20 note), IO (IO5 unmarked), CPUFREQ (its verified metal run filed
  under "Not done").
- "Done" beside a remaining item: FONT-SERVICE (§3.2), ICONS (§10–§12), FILELOCK
  (its QEMU vertical), FIX-RANDOMNESS (the D111 statistic), NEW-SERVICEMANAGER
  (SVC-3's store scan).
- A stale blocker or cross-plan fact: ARXFS-FEC (FEC0 waits on stage 17, which
  is done), ARXFS-MAINTENANCE (WB4 is done), ARCHSUPPORT (A3's D7/D8 note),
  DRIVES (its open-P4 note), FIX-PANICS (the D128 and D79 cites), STRESSTEST
  (ST6's `ramzip` caveat), FIX-HARDWARE-FEATURES (the FIQ probe "depends on
  P1"), COLLECTIONS (C4a against "C0 through C5 landed"), WIRING (the parity
  matrix's aarch64 FDT cell).
- Internal contradictions: APPS (the `chmod` and `chown` registrations), DEVICES
  (UAS in and out of scope), CODEVERIFY and WAFFLE (stages "planned" beside
  landed fixes), NEW-SWITCHBOARD (headings against its ledger), VIEW (the Files
  end-to-end).

## Charter Amendments

Why each `AGENTS.md` rule was added or changed, newest first; the rule itself
lives in the charter.

- **2026-09-17 — §13: every planning file opens with a progress ledger.**
  Status sat wherever each plan put it, so what was built could not be seen or
  cited by id; there is no sweep, and a plan adopts the ledger with its next
  body of work.
- **2026-09-08 — `CAP_CPUFREQ` and `drivers/cpufreq/` (§3, §5.2, §15.18).** A Pi
  4B ran at 600 MHz because nothing asked the firmware for more, and a pinned
  clock harms the whole machine in a way no per-process limit bounds.
- **2026-09-07 — §25: the userland heap resizes its arena in granules.**
  Retention bounds what a process keeps, not a teardown's syscalls (3840
  `mem_unmap`s for a 16 MiB arena); the granule is deliberately not derived.
- **2026-09-03 — §28: an interactive surface answers within a frame.** Every
  desktop freeze found was the same unwritten rule, re-argued each time, and a
  reviewer had nothing to point at.
- **2026-08-30 — §2.15: the gate exemption covers `PLAN.md`, `CLAUDE.md` and
  `AGENTS.md` prose, not only `plans/`.** No stage observes them; only a change
  to the charter's section-label set can alter `charter-cite`.
- **2026-08-25 — §7: the gate is watched to completion.** `ci` outlives an
  agent's per-call cap and a killed foreground call records no status, so the
  rule states the requirement (the real exit status) rather than a mechanism.
- **2026-08-21 — §16.2: `/System/Zoneinfo`.** Compiled IANA rules are read-only
  shipped data, neither settings nor a curated library, and a file stays
  replaceable and out of every binary.
- **2026-08-05 — §2.11: comments are terse, and a file's existing prose is not
  precedent.** Agents matched the exposition they found in a file.
- **2026-08-04 — §16.2, §16.3, §16.5 and the new §16.8: two system program
  stores and a fixed lookup prefix.** It keeps the coreutils surface reviewable
  and makes "a system command cannot be shadowed" structural rather than a
  `PATH` convention.
- **2026-08-03 — §10: every app ships its own icon, SVG preferred.** Fifty
  programs drew one generic picture, and §10 read as requiring PNG.
- **2026-08-02 — §10: raster masters are canonical for illustrative icons, and
  the built-in glyph is mandatory.** Application, file and device icons arrived
  as pictures with no vector form, and no missing file may blank a surface.
- **2026-07-28 — §17.2: the optional `MachineTakeover` Arch HAL slice.** The
  Supervisor's destructive whole-RAM test needs one irreversible per-arch
  operation; it is optional so an unwired port fails closed instead of blocking
  boot.
- **2026-07-27 — §17.2: the `CoreClock` Arch HAL slice.** `CpuCycles` is a fixed
  reference clock that cannot see DVFS, so a live frequency needs a core-clock
  counter.
- **2026-07-21 — §3, §12, §15.18: a first-party Rust boot chain.** The x86_64
  image could not boot real firmware without GRUB, which is external C.
- **2026-07-20 — §7, §23.4: "machine load" is never an excuse for a failing
  test.** Every failure dismissed as load was a real defect the load exposed.
- **2026-07-20 — §23.2 and `lib/sync`: ISR-shared locks must be
  interrupt-safe.** Syscall bodies now run with interrupts enabled, so a plain
  `SpinLock` shared with an ISR self-deadlocks its CPU.
- **2026-07-15 — §17.1: CFQ is the one non-tickless policy, and the default.** A
  Linux-familiar default that never leaves a lone CPU-bound task without an
  armed quantum; EEVDF and MLFQ stay tickless.
- **2026-07-08 — §15.18: the `plans/` jump-sheet.** Capability work was done
  without its plan, because nothing said which plan governs an area.
- **2026-07-07 — §16.3: a user's own program stores, nested filing, and `man`'s
  bounded recursive search.** A user's commands become typeable with no `PATH`
  edit, and help is found wherever a bundle is filed.
- **2026-07-06 — §27: foundational primitives are complete, not minimal.** The
  wait queue shipped as an O(n) `Vec` with only `wake_all`, because §2.3 and §2.4
  had been read as licence to ship a thin core.
- **2026-07-05 — §2.24: fail loud, degrade gracefully** (extended 2026-07-07 to
  the system log). `top` exited silently when an optional view was refused, and
  the runtime's panic handler discarded its message.
- **2026-07-05 — §16.7: system command apps follow GNU coreutils.** Someone who
  knows the GNU tool must find ours familiar; a deviation carries the burden of
  proof.
- **2026-07-04 — §16.2: services are apps.** A separate service packaging would
  be a second, weaker trust path; only PID 1 is compiled in.
- **2026-07-04 — §16.5: bundles are self-contained, and no app code lives in the
  kernel.** Command apps shipped only `Help/` on disk while their `Run` was
  compiled into the kernel, bypassing `appmgr` verification.
- **2026-07-04 — §16.5 and `tools/syshelp`: help is authored in the bundle.**
  Programs embedded their help with `include_bytes!`, and two hand-kept lists
  planted it.
- **2026-07-03 — §3: `lib/cmdres`.** `man` needed the shell's resolution order
  without a userland-to-userland edge or a second copy.
- **2026-07-03 — §16.5: `Documentation/` merged into `Help/`.** Two overlapping
  documentation entries are duplication.
- **2026-07-01 — §16.1: storage is a forest of named roots, and `/` is a
  view.** A volume stays reachable by `id::` when the view or the `System`
  volume is absent, so there is no single-root failure.
- **2026-07-01 — §7: "transient" is not a diagnosis.** A QEMU timeout was waved
  through as a load flake.
- **2026-06-30 — §16.1: the OS never authors legacy POSIX names, and the kernel
  does not police the user.** The VFS refusal was policy with no capability;
  the root's own mode governs `/`.
- **2026-06-30 — §5.2: capabilities stay minimal.** Capabilities were being
  minted ahead of the services that would hold them.
- **2026-06-29 — §26.7: several 100 TB+ drives on a 1 GiB machine.** The
  operating conditions must hold together, not one at a time.
- **2026-06-24 — §2.11, §15.17: no charter-section citations in comments.** A
  citation restates the rule and never the reason.
- **2026-06-24 — §2.22: device logic lives in its driver.** A transitional kernel
  scaffold is not a second consumer, so `lib/vl805` and `lib/pcie_brcm` folded
  into their drivers.
- **2026-06-23 — §17.1: no cooperative dispatch loop.** A 4.3 s in-kernel MMIO
  read with interrupts masked stalled the whole Pi 4.
- **2026-06-23 — §14, §15.16: AI agents never commit or push.** The human
  reviews the working tree first.
- **2026-06-20 — §17.1: the kernel is tickless.** A fixed-frequency tick
  interrupts idle and lone-task CPUs for nothing.
- **2026-06-18 — §2.21: minimal arch-specific code, shared across ports.**
  Common logic was being stranded in one port and re-derived in the others.
- **2026-06-16 — §2.19: no "for now".** Deferring correctness is a defect.
- **2026-06-16 — §2.20: shared code is platform-neutral.** Board names and
  constants were leaking into generic layers.
- **2026-06-16 — §8, §16.2: the driver namespace is a class or bus, never a
  vendor.** A vendor-neutral consumer must find every driver of a class.
- **2026-06-16 — §2.2: no-duplication binds constants.** Stack, MMIO-window and
  canary constants were copied across three `init_spawn*.rs` files.
- **2026-06-16 — §18.6: the driver set is discovered above a minimal floor.** No
  build can enumerate future hardware.
- **2026-06-10 — §24: capacities scale with the machine.** A 2 MiB stack arena
  and a fixed `MAX_CPUS` were cliffs on large machines and waste on small ones.
- **2026-06-09 — §13: plan files are not changelogs.** Landing logs buried the
  current state.
- **2026-06-09 — §2.18: every defect caused or noticed is fixed now, with a
  regression test.** "Pre-existing" and "out of scope" were being used as exits,
  and an escalated defect carried no test obligation.
- **2026-06-07 — §2.13, §2.14, §23: no pre-release compatibility code, delete
  obsolete code, and the review gate.** Nothing has shipped, so compatibility
  seams and dead code only mislead.
