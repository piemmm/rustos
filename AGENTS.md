# AGENTS.md — RustOS Engineering Charter

This document is the **binding contract** for every AI agent (and human contributor)
who works on RustOS. It is **not advisory**. Any pull request, commit, or generated
file that violates these rules is to be rejected and reworked.

If a rule is ambiguous, stop and ask. **Do not guess. Do not invent shortcuts.
Do not "just make it work".**

---

## 1. Project Identity

- **Name:** RustOS
- **Language:** Rust only. No C, no C++, no assembly except where the architecture
  *strictly* requires it (boot stubs, context switches, MMU/TLB primitives). Every
  such file must be justified in a header comment and reviewed.
- **Targets (Tier-1):**
  - `x86_64-unknown-none` (BIOS + UEFI PCs)
  - `aarch64-unknown-none` (Raspberry Pi 3/4/5, generic ARMv8)
  - `riscv64gc-unknown-none-elf` (QEMU virt, SiFive boards)
  - `wasm32-unknown-unknown` (browser, Chrome-class environment)
- **Focus:** Security, correctness, multi-user, multi-core, modular drivers,
  RISC OS-style desktop with a compositing window manager.

---

## 2. Non-Negotiable Rules

These are absolute. They override any local convenience.

1. **No hacks.** If a problem cannot be solved cleanly, raise an issue. Never
   paper over it with a `TODO`, `unsafe { /* trust me */ }`, sleep loop,
   global mutable static, retry-until-it-works, or commented-out test.
2. **No code duplication, ever.** If you find yourself writing similar code
   twice, extract a crate in `lib/` (see §6). Duplication is a review blocker.
3. **No bloat.** Every type, trait, function, file, and folder must justify its
   existence. "Helper" modules that wrap one-liners are forbidden.
4. **No interface creep.** Public interfaces (kernel syscalls, driver traits,
   IPC, ABI) are versioned and frozen on release. Adding a method "to make
   access easier" is forbidden. Extend via a new versioned trait/interface.
5. **All unit tests must pass on every change. No exceptions.**
   - "Pre-existing failure" is not an excuse. Fix it or revert.
   - Disabling, ignoring (`#[ignore]`), or weakening a test is forbidden unless
     the test itself is provably wrong, and only with an accompanying issue
     and replacement test.
6. **Senior-developer quality only.** Code must be reviewed against the same
   bar a senior systems engineer would apply at a kernel vendor. If you would
   be embarrassed to defend a line in a code review, rewrite it.
7. **Security is the default, not a feature.** Every interface, syscall, IPC
   endpoint, and filesystem op is permission-checked. "Open by default" APIs
   are forbidden.
8. **Documentation is part of the code.** A change that updates behaviour
   without updating its docs (rustdoc + the relevant `docs/` page) is incomplete.
9. **No `unwrap()` / `expect()` / `panic!()` in production paths.** They are
   permitted in tests and in clearly-marked unrecoverable boot-time invariants
   (documented with `// SAFETY-INVARIANT:` and reviewed).
10. **`unsafe` requires:**
    - A `// SAFETY:` block immediately above, explaining every invariant.
    - A unit test (or model check) covering the invariants.
    - Encapsulation behind a safe API. No `unsafe` leaks across crate boundaries.
11. **Code must be self-documenting.** Write code clearly enough that it
    explains itself with the comments stripped out: intention-revealing names,
    small single-purpose functions, obvious control flow, and types that make
    illegal states unrepresentable. If a comment is the only thing that makes a
    line understandable, the code is wrong — rewrite the code, do not add the
    comment. Comments are reserved for *why* (rationale, invariants, references,
    `// SAFETY:`), never for restating *what* the code already says. This does
    not relax §2.8: rustdoc and `docs/` pages remain mandatory.

---

## 3. Repository Layout (authoritative)

```
rustos/
├── AGENTS.md            # This file. Binding.
├── PLAN.md              # Staged build plan.
├── README.md            # Short orientation only. No tutorials here.
├── LICENSE
├── Cargo.toml           # Virtual workspace manifest.
├── rust-toolchain.toml  # Pinned nightly + components.
├── .cargo/config.toml   # Per-target build settings.
│
├── kernel/              # The microkernel. One crate per architecture-neutral
│   ├── core/            #   subsystem. No driver code here.
│   ├── mem/             # Allocator, paging, process isolation.
│   ├── sched/           # SMP scheduler.
│   ├── ipc/             # Capabilities, message ports.
│   ├── sec/             # Users, groups, capabilities, MAC.
│   ├── syscall/         # Syscall dispatch + ABI definitions.
│   └── arch/
│       ├── x86_64/
│       ├── aarch64/
│       ├── riscv64/
│       └── wasm32/
│
├── drivers/             # Loadable modules. One folder per device class.
│   ├── display/
│   │   ├── vesa/
│   │   ├── framebuffer/
│   │   └── gpu_virtio/
│   ├── filesystem/
│   │   ├── ext4/
│   │   ├── fat32/
│   │   └── rustfs/      # Native, POSIX-compliant, capability-aware FS.
│   ├── input/
│   ├── network/
│   ├── storage/
│   └── bus/             # pci, usb, virtio, mmio
│
├── lib/                 # Shared no_std crates. The only place for common code.
│   ├── abi/             # Stable user/kernel ABI types.
│   ├── caps/            # Capability primitives.
│   ├── collections/     # no_std collections not in core/alloc.
│   ├── crypto/          # Audited crypto. No hand-rolled primitives.
│   ├── log/             # Structured logging.
│   └── util/            # Strictly justified utilities.
│
├── userland/            # Grouped by <class>/<crate>, mirroring drivers/.
│   ├── system/          # Long-running system services.
│   │   ├── init/        # PID 1.
│   │   └── installer/   # Image installer (partitioning, user creation, naming).
│   ├── session/         # Authentication and session bring-up.
│   │   └── login/       # Text + graphical login (graphical falls back to text).
│   ├── shell/           # Command-line shells.
│   │   └── shell/       # Default POSIX-ish shell with job control.
│   ├── gui/             # Graphical desktop components.
│   │   ├── wm/          # Compositing window manager (RISC OS-style).
│   │   └── iconbar/     # RISC OS-style iconbar.
│   └── apps/            # Default apps. Each app is its own crate.
│
├── docs/                # Long-form documentation (mdBook).
│   ├── src/
│   │   ├── architecture/
│   │   ├── security/
│   │   ├── drivers/
│   │   ├── abi/
│   │   ├── filesystem/
│   │   └── platform/
│   └── book.toml
│
├── tests/               # Cross-crate / integration tests only.
│                        # Per-crate unit tests live in `src/` next to code
│                        # (see §7).
│
├── tools/
│   ├── xtask/           # Build orchestration (cargo xtask ...).
│   ├── mkimage/         # Image builders per platform.
│   └── qemu/            # QEMU run scripts.
│
└── images/              # Output: .iso, .img, .wasm bundles. .gitignored.
```

No file may exist outside this layout. Adding a top-level directory requires
an update to this section.

---

## 4. Kernel Rules

- **Microkernel-leaning.** Drivers run in user space wherever feasible. Only
  scheduling, memory, IPC, capabilities, and the minimum architecture glue
  live in ring 0 / EL1 / M-mode.
- **SMP from day one.** No "single-CPU first, parallelize later" patches.
  All shared state uses explicit synchronization primitives from `kernel/sync`.
- **Memory isolation is enforced by hardware** (page tables / MMU / WASM
  sandboxing). A process can only reach another process's memory through an
  explicit, capability-checked shared-memory IPC object.
- **Allocator requirements:**
  - Per-process heaps, never a global user heap.
  - Guard pages around kernel slabs.
  - Zero-on-free for any allocation that ever held credentials, keys, or
    capability tokens.
  - No `unsafe` global allocator that performs raw pointer arithmetic without
    bounds-checked wrappers.
  - Deterministic OOM behaviour: allocation failure is a `Result`, never a panic.
- **No ambient authority.** Every syscall takes the calling task's capability
  set explicitly; there is no "root can do anything" backdoor in kernel code.

---

## 5. Security Model

### 5.1 Users and Groups

- Every process runs as `(uid, gid, supplementary_gids, capability_set)`.
- `uid = 0` is **not** all-powerful. It is merely the system user; powers come
  from capabilities, not from the uid.
- Groups are first-class objects with their own ACL.
- Users and groups are persisted in `/System/Security/Users` and
  `/System/Security/Groups` (see §16 for the on-disk filesystem layout).
  Both are themselves protected by capabilities.
- There is no `/etc`. Anything that on a POSIX system would live under
  `/etc` lives under `/System/Settings/` (machine-wide) or under the
  per-user `/Users/<name>/Settings/` (user-scoped). See §16.

### 5.2 Capabilities

- Capabilities are unforgeable kernel-issued tokens. Examples:
  `CAP_FS_MOUNT`, `CAP_NET_RAW`, `CAP_DRV_LOAD`, `CAP_USER_ADMIN`,
  `CAP_TIME_SET`, `CAP_IPC_BIND_PRIVILEGED`.
- A process's capability set is the intersection of its user's grants and its
  executable's manifest request. Manifests are signed.
- Capabilities can be **delegated** (a subset, never a superset) and **revoked**.
- IPC endpoints declare the capabilities required to call each method. The
  kernel enforces this at dispatch time; the receiver does not need to re-check.

### 5.3 Filesystem permissions

- POSIX mode bits **plus** ACLs **plus** capability gates.
- Every inode stores: owner uid, owning gid, mode, ACL, and an optional
  capability requirement (e.g. "reading this file requires `CAP_AUDIT_READ`").
- Mounts have their own permission policy (`nosuid`, `nodev`, `noexec`,
  `ro`, etc.) and the installer's default layout uses them aggressively.
  The concrete defaults for `/System`, `/Users`, `/Apps`, and `/Storage`
  are defined in §16.

### 5.4 Mandatory rules for every IPC/syscall/driver entry point

1. Identify the caller (kernel-provided, not caller-supplied).
2. Check capabilities **before** touching any state.
3. Validate **every** input. No "trusted caller" shortcuts.
4. Log security-relevant decisions through `lib/log` with a stable event ID.
5. Fail closed.

---

## 6. Code Re-use and Common Libraries

- All shared code lives in `lib/`. If two crates need it, it goes there.
- A `lib/*` crate must:
  - be `no_std` unless explicitly justified,
  - have its own unit tests,
  - have rustdoc on every public item,
  - declare a stability tier in its `README.md` (`experimental`, `stable`, `frozen`).
- Adding a `lib/*` crate requires updating §3 and `PLAN.md`.

---

## 7. Testing Rules

- **Mirror layout.** Unit tests live next to the code they test:
  - `kernel/mem/src/allocator.rs` → tests in the same file under `#[cfg(test)] mod tests`
    *or* in `kernel/mem/src/allocator_tests.rs` if the file would otherwise exceed
    500 lines.
  - Integration tests for a crate live in `<crate>/tests/`.
  - Cross-crate / end-to-end tests live in the top-level `tests/`.
- **Every change runs the full test matrix.** `cargo xtask test` runs:
  1. `cargo test` for all host-testable crates.
  2. `cargo test --target <each kernel target>` where applicable (using QEMU
     via `tools/qemu`).
  3. `cargo clippy -- -D warnings` (warnings are errors).
  4. `cargo fmt --check`.
  5. `cargo deny check` (license + advisory audit).
  6. `cargo doc --no-deps` (doc build must succeed; broken links fail the build).
- **A failing test blocks the change.** Whether or not the failure existed
  before is irrelevant.
- **No flaky tests.** A test that fails intermittently is a bug; fix the test
  or fix the code, never retry.
- **Coverage targets** (enforced by `cargo xtask coverage`):
  - `kernel/sec`, `kernel/mem`, `kernel/ipc`, `lib/caps`, `lib/crypto`: **≥ 95%**
  - All other kernel crates: ≥ 85%
  - Drivers and userland: ≥ 75%

---

## 8. Drivers

- A driver is a crate under `drivers/<class>/<name>/`.
- It implements the trait(s) defined in `lib/abi/src/driver/<class>.rs`.
- It exposes a single `pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError>`
  entry point. Nothing else is public.
- Drivers are **loadable and unloadable at runtime** unless the hardware
  forbids it (document why in the crate's `README.md`).
- Drivers run in user space by default. A driver that must run in-kernel
  declares `kind = "in-kernel"` in its manifest and requires `CAP_DRV_KERNEL`
  at load time.
- Every driver has:
  - `README.md` describing supported hardware, limitations, and required capabilities.
  - Unit tests against a mock bus/host.
  - At least one QEMU integration test where the hardware is emulable.

---

## 9. Executable ABI

- One binary format: **`rxe`** (Rust eXecutable Envelope) — an ELF-derived
  format extended with a signed manifest section describing:
  - Required capabilities.
  - Target ABI version (`abi-vN`).
  - Linked syscall interface hashes (refuse to load on mismatch).
- The ABI is versioned. `abi-v1` once shipped is immutable. New behaviour
  ships as `abi-v2`.
- Userland-to-kernel transitions use a single, documented syscall table per
  architecture. The table lives in `kernel/syscall/src/table.rs` and is
  generated from `lib/abi/src/syscalls.rs` — do not edit either by hand
  without updating the other; `cargo xtask abi-check` enforces this.

---

## 10. Desktop and Window Manager

- RISC OS-style: iconbar at the bottom (or configured edge), filer-style file
  manager, third-mouse-button menus, drag-and-drop save model.
- Compositing window manager (`userland/gui/wm`). All compositing happens in
  user space; the kernel only ships framebuffer access through a capability.
- Login: `userland/session/login` always starts in text mode and offers to launch the
  graphical session. If no graphics driver loads, the graphical option is
  hidden — never crashed, never errored.

---

## 11. Installer

- `userland/system/installer` runs on first boot of any image.
- It must:
  1. Prompt for system name (hostname).
  2. Prompt for the first user (username, password, full name, primary group).
  3. Offer the secure default layout defined in §16:
     - Encrypted root (LUKS-equivalent using `lib/crypto`).
     - `/System` mounted read-only (the only writable paths inside it
       are `/System/Logs` and `/System/Settings`, and they are mounted
       `nosuid,nodev,noexec`).
     - `/Users` mounted `nosuid,nodev`.
     - `/Apps` mounted `nosuid,nodev` (application binaries are
       capability-gated, not setuid).
     - `/Storage` mounted `nosuid,nodev,noexec` by default; per-volume
       overrides are capability-gated.
  4. Allow expert mode for manual partitioning, but the default is the
     secure layout. Expert mode may not introduce legacy POSIX top-level
     directories (`/etc`, `/home`, `/usr`, `/var`, `/proc`, `/lib`,
     `/bin`, `/sbin`, `/opt`, `/root`, `/tmp`, `/dev`); those names are
     reserved and refused by the installer.
  5. Generate a per-installation machine ID and signing key for the local
     capability authority.
- The installer is the same binary on every platform; platform specifics
  (e.g. EFI vars, Pi config.txt) live behind a `Platform` trait in
  `lib/abi/src/platform.rs`.

---

## 12. Build and Images

- `cargo xtask build --target <platform>` produces:
  - `images/rustos-x86_64.iso` (hybrid BIOS/UEFI, USB-writable)
  - `images/rustos-aarch64-rpi.img` (SD-card writable)
  - `images/rustos-riscv64.img`
  - `images/rustos-web/` (static tree for Apache/Nginx, contains `.wasm`,
    `.html`, `.js` glue, and a service worker)
- Image builders live in `tools/mkimage`. They are pure Rust; no shelling out
  to `mkfs`, `parted`, `xorriso`, etc., except via `tools/mkimage`'s
  audited wrappers in `tools/mkimage/src/extern_tools.rs`. Every external
  invocation is checksummed and version-pinned.

---

## 13. Documentation Rules

- Every public item has rustdoc.
- Every subsystem has a page under `docs/src/`.
- Documentation is reviewed and updated **in the same commit** as the code
  it describes.
- No "aimless waffle". If a paragraph does not change the reader's ability
  to use or maintain the system, delete it.
- `cargo xtask docs-check` runs:
  - `cargo doc` with `RUSTDOCFLAGS="-D warnings"`,
  - mdBook build,
  - link checker,
  - a "stale doc" check that fails if a file in `docs/src/` references a
    symbol that no longer exists.

---

## 14. Commit / PR Discipline

- One logical change per commit.
- Commit message format:
  ```
  <area>: <imperative summary, ≤ 72 chars>

  <body explaining *why*, wrapped at 72>

  Tests: <what you ran>
  Docs:  <what you updated>
  ```
- A PR is mergeable only when:
  - `cargo xtask ci` passes locally and in CI,
  - all reviewer comments are resolved,
  - `PLAN.md` is updated if a stage was advanced.

---

## 15. Instructions Specifically for AI Agents

You are not exempt from any rule above. In addition:

1. **Do not be lazy.** If the task is large, decompose it and complete each
   piece properly. Do not stub functions with `todo!()` and call the work done.
2. **Do not invent APIs.** If you need an interface that does not exist,
   propose it in `PLAN.md` and wait for approval, or implement it fully —
   including tests and docs — as part of the same change.
3. **Do not silence tests, warnings, or lints.** Fix the underlying problem.
4. **Do not duplicate code to avoid a refactor.** Refactor.
5. **Do not add "convenience" wrappers** unless they are used in at least two
   independent places and documented.
6. **Run the full test suite** (`cargo xtask test`) before reporting a task
   complete. Quote the actual output.
7. **State your assumptions** at the top of any non-trivial change. If an
   assumption cannot be verified from the repository, stop and ask.
8. **Never** edit generated files, `target/`, or `.idea/` content as part of
   a feature change.
9. **Never** weaken the security model "to get the test to pass". The test
   is correct; your code is wrong.
10. If you are about to write `// HACK`, `// FIXME later`, `// works for now`,
    or `#[allow(...)]` without a justification comment — **stop**. Rework
    the change.

---

## 16. OS Filesystem Layout (authoritative)

This section governs the **installed system's** on-disk layout, not the
source repository (the source layout is §3). It is binding.

### 16.1 Top-level directories

RustOS has **exactly four** top-level directories. Anything else is a
defect.

```
/
├── System/    # All OS-provided files. Read-only at runtime.
├── Users/     # One subdirectory per user account.
├── Apps/      # Installed applications. One bundle per app.
└── Storage/   # Mount points for removable / extra volumes.
```

The following legacy POSIX names are **reserved and forbidden** as
top-level directories: `/etc`, `/home`, `/usr`, `/var`, `/proc`, `/sys`,
`/lib`, `/lib64`, `/bin`, `/sbin`, `/opt`, `/root`, `/tmp`, `/dev`,
`/mnt`, `/media`, `/run`, `/boot`. The kernel filesystem layer refuses
to create them; the installer refuses to lay them out; any driver or
userland code that hard-codes one of these paths is a defect.

There is **no `/proc`** and **no `/sys`**. Live system information is
exposed exclusively through the System Information API (§16.6).

### 16.2 `/System`

`/System` contains every file that ships as part of the OS. It is
mounted read-only at runtime. The installer and the updater are the
only components permitted to mutate it, and only by re-mounting it
read-write under `CAP_SYSTEM_UPDATE`.

Authoritative subdirectories:

```
/System/
├── Kernel/      # Kernel image(s) and boot artifacts for the platform.
├── Drivers/     # Loadable drivers (rxe modules) shipped with the OS.
├── Libraries/   # The OS-provided shared libraries (see §16.4).
├── Fonts/       # System fonts.
├── Graphics/    # WM, compositor assets, cursors, icons.
├── Audio/       # System audio service assets.
├── Network/     # Network stack configuration and service binaries.
├── Security/    # Users, Groups, capability authority, keys, policy.
│   ├── Users    # User database (see §5.1).
│   ├── Groups   # Group database (see §5.1).
│   ├── Keys/    # Local capability-authority signing material.
│   └── Policy/  # MAC and capability policy.
├── Printing/    # Print spooler and drivers' user-space services.
├── Logs/        # Append-only structured logs (writable; nosuid,nodev,noexec).
├── Settings/    # Machine-wide settings (writable; nosuid,nodev,noexec).
└── Services/    # Long-running system services (init manifests, etc).
```

Adding a new top-level subdirectory under `/System` requires updating
this section and `PLAN.md`. Subdirectories outside this list are a
defect.

`/System/Logs` and `/System/Settings` are the only writable paths
beneath `/System`. They are mounted `nosuid,nodev,noexec` and are
capability-gated (`CAP_LOG_WRITE`, `CAP_SETTINGS_WRITE`).

### 16.3 `/Users`, `/Apps`, `/Storage`

- `/Users/<username>/` is the only place user-owned files live. It
  contains at least `Documents/`, `Settings/`, `Library/` (per-user
  caches/state, **not** shared libraries), and `Desktop/`. The shape is
  fixed; applications may not invent sibling directories at this level.
  `/Users` is mounted `nosuid,nodev`.
- `/Apps/<Name>.app/` is the only place applications live (see §16.5).
  `/Apps` is mounted `nosuid,nodev`. Apps acquire privilege through
  capabilities declared in their manifest, never through setuid.
- `/Storage/<volume>/` is where mounted volumes appear (removable media,
  extra disks, network shares). Default mount flags are
  `nosuid,nodev,noexec`; relaxations require `CAP_FS_MOUNT_RELAX` and
  are recorded in the audit log.

### 16.4 Shared libraries

The OS ships a **closed, curated set** of shared libraries under
`/System/Libraries/`. They are the only shared libraries on the system.
The permitted classes are:

- Windowing / compositor client
- Font rendering
- Image decoding
- Media (audio/video) decoding and playback
- Archive extraction
- Printing
- TLS / cryptography (via `lib/crypto`)
- Networking (sockets, DNS, HTTP client)

Adding a new class of OS-provided shared library requires an update to
this list **and** to `PLAN.md`. "Convenience" libraries are forbidden.

Applications **must be self-contained**. They may not install shared
libraries outside their own bundle, and they may not depend on shared
libraries other than the curated `/System/Libraries/` set. An
application that needs additional code links it statically or vendors
it privately into `Libraries/` inside its own bundle (§16.5);
statically-linked code is preferred because it leaves responsibility
for security updates squarely with the application developer.

The dynamic loader refuses to resolve a shared-library reference that
points anywhere other than the requesting app's own `Libraries/` or
`/System/Libraries/`.

### 16.5 Application bundles (`.app`)

An installed application is a directory named `<Name>.app` placed
directly under `/Apps/`. The bundle layout is fixed:

```
/Apps/Example.app/
├── AppInfo            # Signed manifest. Required. See below.
├── Run                # Entry-point rxe binary. Required.
├── Code/              # Additional rxe binaries / plugins.
├── Libraries/         # Private shared libraries used only by this app.
├── Resources/         # Images, locales, UI definitions, etc.
├── DefaultSettings/   # Read-only defaults; copied into the user's
│                      # /Users/<u>/Settings/<Name>/ on first launch.
└── Documentation/     # Bundled docs, opened by the help viewer.
```

Exactly these names are permitted at the top of a bundle; additional
entries are a packaging defect and the loader will refuse the bundle.

`AppInfo` is the application manifest. It is a signed document (see §9)
and declares at minimum:

- Bundle identifier, human-readable name, version.
- Target ABI version (`abi-vN`) and required syscall hashes.
- The exact set of capabilities the app requests (§5.2). The kernel
  grants only the intersection of these with the launching user's
  grants; ambient authority is forbidden (§4).
- Declared MIME / file-type associations.
- The signer's identity and signature over the bundle contents.

Apps may not write outside their own per-user state
(`/Users/<u>/Settings/<Name>/` and an app-scoped `Library/<Name>/`
cache directory). All other writes require a user-mediated capability
(e.g. a file picker handing the app a one-shot file capability).

### 16.6 System Information API

Because there is no `/proc` or `/sys`, every piece of information that
would have lived there is exposed through a single, versioned,
capability-checked API: the **System Information API** (`sysinfo`).

- The API is defined in `lib/abi/src/sysinfo.rs` and served by a
  user-space system service under `/System/Services/`.
- Each query is a typed request returning a typed response; there is
  no free-form text scraping interface. Adding a query requires the
  same ABI discipline as a syscall (§9): versioned, hashed, frozen
  on release.
- Every query declares the capability it requires. Unprivileged queries
  (e.g. "list my own processes") need none; privileged queries (e.g.
  "list all processes system-wide", "read kernel memory stats") require
  capabilities such as `CAP_SYSINFO_GLOBAL` or `CAP_SYSINFO_KERNEL`.
- A single command-line tool, `sysinfo`, in `userland/shell/`, exposes
  the same API to the terminal. It does **not** open files in a virtual
  filesystem; it calls the API. There is no privileged path that
  bypasses the capability check.

Any in-tree code that needs runtime system data uses this API.
Fabricating a `/proc`-style virtual filesystem, even "just for
compatibility", is forbidden.

---

Violation of any rule in this document is a defect, regardless of whether
the code compiles or the tests pass.
