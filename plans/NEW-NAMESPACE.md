# NEW-NAMESPACE.md — boot namespace assembly from signed policy

Binding under `AGENTS.md`. This plan replaces the compiled-in boot mount
topology with a signed, per-installation **namespace policy** read off the
read-only `/System` volume, and splits the two concepts the mount table
currently fuses — a **volume attachment** and a **view projection**.

It completes the target state the binding storage spec already names
(`docs/src/filesystem/drives.md` §21): *"machine aliases then rebind from the
single root's subtrees to independent `id::` volume roots without changing the
resolver contract."* The resolver contract, the path grammar, and the alias
model are unchanged; only how the bindings come into being changes.

---

## 1. What exists today

**The hardcode.** `kernel/tairix-kernel/src/system_mount.rs::system_vfs()`
builds the whole boot topology from two compiled-in definitions:

- `kernel/core/src/fs/vfs.rs::Vfs::with_default_layout` — a fixed six-entry
  array of `(path, MountFlags)` pairs (`/System`, `/System/Logs`,
  `/System/Settings`, `/Users`, `/Apps`, `/Storage`) over a default-flagged
  root mount from `MountTable::new`, plus the in-RAM directory nodes driven by
  `ROOT_TEMPLATE`.
- `system_vfs()` — attaches two driver handles to those paths:
  `SYSTEM_MOUNT_HANDLE` (the read-only `ARXFSSystem` volume) and
  `ROOT_VOLUME_HANDLE` (the encrypted `ARXFSRoot` volume, rebased onto five
  same-named subtrees).

The volume that backs each path is chosen by `parse_partition_table` +
`first_of_type(PartitionType::ARXFSSystem)` — a positional scan that trusts a
GPT type byte.

**The machinery that should be doing this already exists.**

- `drivers/storage/volmgr` — the user-space volume-manager policy driver:
  probes the partition table (`lib/partition`), probes each extent for a
  filesystem signature (`lib/fsprobe`), derives a deterministic catalog name,
  and asks the kernel to attach through the `CAP_FS_MOUNT`-gated, audited
  `volume_attach` syscall. This is already the path every *hot-plug* volume
  takes. The boot volumes are the exception that bypasses it.
- `tairix_abi::volume::VolumeAttachRequest` — endpoint, window, extent,
  `fstype`, catalog name. Already expressive enough to describe a boot volume.
- `tairix_abi::driver_store::SystemConfigFile` — a **closed whitelist** of
  files the `/System` store service reads on a bootstrap client's behalf
  *before the encrypted root is unlocked*. Already the sanctioned pre-unlock
  read path (`system.conf`, `network.conf`, the service enrolment record).
- `kernel/core::fs::volumes::VolumeForest` — publishes each attached volume's
  stable identity so `id::<uuid>/path` resolves.
- `unlock_orchestrate.rs::finish_unlock` — runs the driver-store serve loop
  and the unlock prompt as **two independent preemptive tasks** over one
  `'static`-leaked disk. Because the store is served independently of the
  user-data passphrase, input drivers load in user space *before* the prompt
  needs a keystroke. This is correct and is preserved unchanged.

`abi-v1` is **not** frozen, so the ABI changes below land in place — no `v2`,
no shim (§2.13).

---

## 2. The three conflated concerns

`system_vfs()` fuses three decisions that want three different answers:

| Concern | Today | Target |
|---|---|---|
| Which volumes exist | positional GPT type scan | probe + superblock identity (NS-2) |
| What each volume is for | inferred from the GPT type byte | named by `id::<uuid>` in signed policy (NS-3) |
| What the namespace looks like | a `const` array in kernel Rust | signed policy, applied by a user-space manager (NS-4) |

The second is the dangerous one: a GPT type byte is attacker-writable
metadata, and "the partition claiming to be root *is* root" is not a property
that survives a second disk.

---

## 3. Attachment is not projection

The mount table has one concept where there are two.

- **Attachment** — a filesystem driver bound to a storage object, producing a
  volume root with a stable `id::` identity. Carries capacity, availability,
  and medium. Two exist on a default install.
- **Projection** — a view path bound to (attachment, subtree) with
  **restricted** flags. Carries no capacity of its own; it is a window onto an
  attachment. Seven exist on a default install, one per view path including
  `/` itself, so the view is built uniformly from projections rather than
  having the root volume placed by a second mechanism.

Collapsing them produces the visible symptom: `mount_snapshot`
(`kernel/core/src/fs/mounted.rs:1463`) emits seven `MountRecord`s for two
volumes — six of them the same volume, every one reporting identical
capacity — and `df` carries a `seen_sources` string-comparison heuristic
(`userland/apps/df/src/client.rs:117`) to guess which rows are real.
`sysmon`'s storage panel (`userland/apps/sysmon/src/app.rs:576`) has no such
heuristic and shows all seven.

Splitting the model deletes the heuristic rather than copying it into the
other consumers (§2.2).

**It also makes a documented invariant enforceable.** `drives.md` §9 and §15
require that a view binding *"may only preserve or restrict"* its source
root's flags, and that relaxing needs `CAP_FS_MOUNT_RELAX`. That capability
has no definition and no enforcement point anywhere in the tree today — it
cannot have one, because `system_vfs()` writes flags directly into
`MountTable::mount()` and there is no operation to gate. Once a projection is
created by a *monotone narrowing* of its attachment's flags, the invariant is
structural: nothing can hand `/Storage` an exec bit.

---

## 4. Where the policy lives, and why it is not in the encrypted volume

Putting the table in the encrypted root is circular: reading it requires
unlocking the volume that holds it, so a second, smaller hardcoded table would
be needed to describe how to reach the first.

The circularity dissolves because **the namespace topology is not secret; it
only needs to be authentic.** Which volume is `/Users` is not a secret. The
data on it is. So the policy lives on the read-only `/System` volume, signed:

```text
/System/Security/Policy/namespace          # the policy document
/System/Security/Policy/namespace.sig      # detached Ed25519 signature
```

`drives.md` §7 already fixes this location: *"Persistent alias policy is
signed (`System:/Security/Policy`) and loaded at boot."*

**Why `Security/Policy` and not `Settings`.** The service enrolment record
lives under `/System/Settings` because enablement is configuration. The
namespace policy is not: it decides which volume the `System:` alias — hence
every executable the system loads — comes from. That is a statement about
trust, and it belongs beside the MAC and capability policy.

**Signing.** The policy names *this machine's* volume identities, so it is
signed by the per-installation capability-authority key the installer mints
(§11.5), whose public half is under `/System/Security/Keys/`. Verification
reuses `lib/crypto`'s Ed25519 path — the same primitive `lib/appload` uses for
bundle manifests, never a second verifier.

**Where the chain terminates.** The policy is anchored by the installation
key; the installation key is anchored by the `/System` volume the measured
loader chose. With a TPM (`plans/TPM.md`) that terminates in a measurement.
Without one it terminates in physical possession of the disk — which is the
honest answer and is already scoped out by §19.9. This plan does not pretend
otherwise.

**No fallback.** A missing, unverifiable, or malformed policy does **not**
fall back to a built-in layout. A fallback that activates on signature failure
is an attack: corrupt one byte of the signature and get the permissive
default (§5.4).

Verification happens in `nsmgr`, in user space, because that is where policy
belongs — which means the refusal happens *after* the pre-boot Supervisor
window has closed. So the refusal path is not "drop to Supervisor": `nsmgr`
audits the refusal and exits non-zero, PID 1 starts a **recovery session**
bound to `/System` only, and no user volume is attached and no login is
offered. The Supervisor remains what it is — the pre-user-space facility for
a failure the kernel itself hits (no `/System`, no identity match), reachable
at the ESC boot window and not afterwards.

---

## 5. Policy content

The document is parsed by the existing `lib/sysconfig` engine — the same
`key = value` store `/System/Settings/Configuration/system.conf` uses, not a
new format (§2.2). Two record kinds:

```text
# An attachment: a volume this machine expects, named by durable identity.
attach.system.id       = b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001
attach.system.fstype   = arxfs
attach.system.key      = well-known
attach.system.flags    = ro

attach.root.id         = 4c1d9a02-77b3-4a19-9f0e-2ab6d5c31e77
attach.root.fstype     = arxfs
attach.root.key        = passphrase
attach.root.flags      = rw

# A projection: a view path onto an attachment's subtree, flags narrowed.
project./              = root:/
project./System        = system:/
project./Users         = root:/Users        nosuid,nodev
project./Apps          = root:/Apps         nosuid,nodev
project./Storage       = root:/Storage      nosuid,nodev,noexec
project./System/Logs   = root:/System/Logs  nosuid,nodev,noexec
project./System/Settings = root:/System/Settings nosuid,nodev,noexec
```

Binding rules, all fail-closed:

- An attachment is named **only** by `id::<uuid>`. A policy naming a partition
  index or a GPT type is refused.
- `key` selects the key provenance (`well-known`, `passphrase`, and — with
  `plans/TPM.md` — `sealed`). It never carries key material.
- A projection's flags must be a **subset-or-equal** narrowing of its
  attachment's flags. An absent flag list means *preserve* (which is why
  `project./System` needs none — its attachment is already `ro`). A widening
  is refused unless the caller holds `CAP_FS_MOUNT_RELAX`, and is audited
  (`fs.flags.relax.{allow,deny}`).
- Every projection's first path component is validated against
  `ROOT_TEMPLATE`: a policy naming a top-level view entry outside those four
  is refused, so §16.1's "exactly four" is enforced structurally rather than
  by convention.
- The document carries the machine ID (`/System/Security/MachineId`); a policy
  signed for another machine is refused. Rollback to an older *validly signed*
  policy for the same machine is defeated by measurement where a TPM exists,
  and is out of scope without one (§19.9), as above.

A multi-volume install is the same document with more `attach.*` stanzas and
projections pointing at different attachments — **no kernel change**, which is
the whole point.

---

## 6. Work items

### NS-1 — Split attachment from projection

- `kernel/core/src/fs/mount.rs`: `MountPoint` splits into `Attachment`
  (driver handle, volume id, source, fstype, flags) and `Projection` (path,
  attachment, `backing_subtree`, narrowed flags, permission template).
  Longest-prefix resolution walks projections; capacity/health/medium come
  from the attachment.
- Projection construction takes the narrowing as its only flag input, so
  widening is unrepresentable without the explicit relax path.
- **Mint `CAP_FS_MOUNT_RELAX` here, with its enforcement point and its live
  holder** — never ahead of them (§5.2).
- `lib/abi/src/sysinfo.rs`: `MountRecord` becomes attachment-only; add
  `ViewBindingRecord` and the `VIEW_LIST` query. Regenerate the C headers
  (`cargo xtask c-header --write`).
- **A `ViewBindingRecord` carries its attachment's volume id**, so a consumer
  resolving a *path* joins projection → attachment in one step. Without this
  `df <path>` and `stat` break: both answer "which mount covers this path?",
  which is a projection question whose answer must report attachment figures.
- Consumers: `sysmon`, `switchboard`, and `fstree` read attachments; `df` and
  `stat` read projections to find the covering path and report through the
  join; `mount` reads both, since it is the `mount(8)` view and must show
  bindings.
- **Delete** `df`'s `select_all` duplicate-source heuristic and its
  `pseudo_or_duplicate_mount` omission record. Capacity-less handling stays —
  under the new model that means a genuinely unreportable volume, which is a
  different and still-honest case.
- `df` default lists attachments; `df -a` adds projections. This *improves*
  coreutils fidelity (§16.7) rather than bending it — GNU `df -a` is likewise
  the flag that reveals bind mounts.

### NS-2 — Bind by volume identity, not partition type

- The kernel locates the `/System` volume by the UUID the boot handover
  carries, probing candidate extents and matching each ARXFS superblock
  identity. The GPT type orders the probe; it never decides.
- `plans/BOOTLOADER.md` gains the handover field. The loader already read the
  kernel off that volume, so it knows the identity.
- **QEMU's `-kernel` path has no loader** (§12 keeps it as the firmware-free
  test path), so the handover field is legitimately absent there. Absent
  identity does not mean "pick the first": the kernel probes every candidate
  extent and proceeds only when **exactly one** carries an ARXFS `/System`
  superblock. Zero or more than one fails closed to the Supervisor with the
  candidates listed. Deterministic, and it keeps every QEMU vertical working
  without weakening the firmware path.
- A handover UUID that matches nothing fails closed the same way — a named
  identity is never silently downgraded to the probe.

### NS-3 — The signed namespace policy

- `SystemConfigFile` gains `NamespacePolicy`; its doc widens from
  "`/System/Settings/` configuration files" to "whitelisted pre-unlock files
  off the read-only `/System` volume". The set stays closed.
- Signature verification through `lib/crypto` against the installation key.
- `lib/sysconfig` gains the `attach.*` / `project.*` schema and its
  fail-closed validation (§5).
- `tools/mkimage` writes and signs the policy at image build; the installer
  writes and signs it at install (§11). `AGENTS.md` §16.2's `Policy/`
  description is updated in this stage, when the file starts existing.

### NS-4 — The namespace manager (`userland/system/nsmgr/`)

- A new system service bundle under `/System/Services/`, started by PID 1.
  Distinct from `volmgr`: `volmgr` is a per-node *driver* that probes one
  storage unit, `nsmgr` is the policy engine that turns the signed document
  into `volume_attach` + the new `view_bind` calls.
- Holds `CAP_FS_MOUNT` and nothing else it does not need. Every request is
  re-validated kernel-side; `nsmgr` is policy, never authority (§4).
- Adds `userland/system/nsmgr/` to §3 and to `PLAN.md` in the stage that
  creates it.

### NS-5 — Root-volume unlock moves to user space

The root volume is attached *after* the driver store is reachable, so it fails
the §18.6 bootstrap-floor test and has no business being an in-kernel driver.

- Root ARXFS becomes an ordinary discovered user-space driver, matched and
  spawned through the normal `devmgr` path.
- The passphrase prompt becomes a user-space unlock agent on a seat, using the
  input and display drivers that are already loaded by then. It can be
  graphical.
- The passphrase and the derived key never enter ring 0.
- **Delete** `unlock_body`, `KthreadConsoleRead`, `SecretFeedback`,
  `WritableStateSink`, and `register_writable_state`.

**Recovery carve-out (justified floor entry).** The pre-boot Supervisor
(`plans/NEW-SUPERVISOR.md`) runs before any user space exists, so its `mount`
command keeps an in-kernel key derivation. It is justified because there is no
user space to delegate to at that point, it is reachable only by a physically
present operator at the ESC boot window, the derived key is zeroed immediately
after the volume is attached, and it is **not** on the normal boot path. The
normal path never derives a key in ring 0.

### NS-6 — Delete the hardcode

- **Delete** `Vfs::with_default_layout`, `system_vfs()`, `ROOT_VOLUME_HANDLE`.
- The kernel attaches exactly one volume: `/System`. Everything else arrives
  through `volume_attach` / `view_bind` from `nsmgr`.
- `ROOT_TEMPLATE` survives **only** as the §16.1 validation set for policy
  (the four permitted root-view names), not as a layout generator.
- `resolve_machine_alias` resolves against the live projection table instead of
  the template.

---

## 7. Boot sequence after this plan

1. Loader hands off, carrying the `/System` volume identity.
2. Kernel brings up the bootstrap-floor disk and attaches `/System` by
   identity. **This is the only mount the kernel performs.** Its integrity
   rests on the ARXFS record checksums and on the loader's measurement of the
   volume it chose — the volume itself is not a signed object, and this plan
   does not claim it is. What *is* signature-checked off it is each driver
   bundle (already) and the namespace policy (NS-3).
3. Kernel spawns PID 1.
4. PID 1 starts `devmgr`; `devmgr` reads the driver store off `/System` and
   loads input and display drivers. (Unchanged: the store is served
   independently of the passphrase, so this happens before any prompt.)
5. PID 1 starts `nsmgr`; it reads and verifies the namespace policy.
6. `nsmgr` attaches every `key = well-known` volume and applies its
   projections.
7. For a `key = passphrase` volume, `nsmgr` starts the unlock agent, which
   prompts on the seat. On success the root ARXFS driver attaches the volume.
8. `nsmgr` applies the remaining projections; the `/` view is complete.
9. `login`.

Failure branches: a kernel-side failure at step 2 (no `/System`, zero or
ambiguous identity candidates) reaches the Supervisor. A policy failure at
step 5 reaches the `/System`-only recovery session — steps 6 to 9 do not run,
and no login is offered.

---

## 8. Threat model

| Vector | Answer |
|---|---|
| Partition-type spoofing (`first_of_type`) | NS-2: identity from the superblock; type is a probe hint only. |
| Policy tampering | Ed25519 signature; anchored at the installation key, measured where a TPM exists. |
| Policy substitution from another machine | Machine-ID binding in the document. |
| Signature-failure downgrade | No fallback layout. `/System`-only recovery session, no login. |
| Flag relaxation by a future edit | Structurally unrepresentable; widening needs `CAP_FS_MOUNT_RELAX` and is audited. |
| Fifth root-view entry smuggled in via policy | Validated against `ROOT_TEMPLATE`; refused. |
| Passphrase or derived key exposed in ring 0 | NS-5 removes it from the normal path; recovery carve-out is justified and zeroes. |
| A broken projection table hiding a healthy volume | Attachments carry `id::` independently, so `drives.md` §4 invariant 3 is strengthened, not weakened. |

---

## 9. Required tests

- **Model (NS-1).** A projection may not widen its attachment's flags without
  the capability; the relax path is audited; `MOUNT_LIST` reports one record
  per attachment and `VIEW_LIST` one per projection; a default single-volume
  install yields two attachments and seven projections.
- **Consumers (NS-1).** `df` and `sysmon` show one row per volume with no
  dedup heuristic in the code path; `mount` shows attachments and bindings.
- **Identity (NS-2).** A disk whose GPT type bytes are permuted still resolves
  the correct `/System` by superblock identity; a disk with two
  `ARXFSSystem`-typed partitions and one matching UUID resolves
  deterministically; no match fails closed to Supervisor rather than picking.
- **Policy (NS-3).** A valid policy applies; a policy with a bad signature, a
  foreign machine ID, a widening projection, a fifth root-view entry, a
  partition-index attachment, or a truncated body is each refused with its own
  typed error and leaves only `/System` attached.
- **Multi-volume (NS-4).** A policy with a separate `/Users` volume produces
  three attachments and no bind-style projection for `/Users`, with **no
  kernel change**.
- **Unlock (NS-5).** The passphrase never appears in a kernel allocation; a
  QEMU vertical drives the user-space agent end to end; the Supervisor
  recovery path zeroes its derived key.
- **Fail-closed boot.** Absent `/System` and an ambiguous identity probe each
  reach the Supervisor without panicking; absent and unverifiable policy each
  reach the `/System`-only recovery session with no user volume attached and
  no login offered.
- **No-loader path (NS-2).** A `-kernel` boot with one ARXFS `/System`
  candidate proceeds; with two it fails closed rather than choosing.

---

## 10. Status

`planned` — no stage started. Ordering is NS-1 → NS-2 → NS-3 → NS-4 → NS-5 →
NS-6; each lands complete with its tests and docs (§2.19 — no partial stage
reported as done). NS-1 is independent of the rest and fixes the visible
duplicate-mount reporting on its own.
