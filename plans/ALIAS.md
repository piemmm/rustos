# ALIAS.md - TAIRiX Resource Alias and Selector Specification

Status: normative design input for implementation. The single shared
reference **parser** is landed as `lib/resref` (`tairix-resref`): the one
definition of how the grammar in §5 is lexed and validated into typed values
(`ResourceRef` and the §16.2 conceptual types it covers), spelling only. The
**resolver** is split by the layer that owns each resource, not lumped behind
one syscall. The kernel-side resolver and descriptor-producing path
(`resource_open` over `kernel/core::resource`) serve only *kernel-owned*
backings: it resolves a reference through `lib/resref` and mints a
resource-backed descriptor (from the same per-process number space as
`fs_open`), serving `sys:random` and `sys:null` fail-closed and unprivileged.
The `info:`/`stats:` namespaces are resolved in **userspace**
(`lib/procinfo::resolve` + the §14 `resinfo` envelope) over the System
Information API's typed queries — never by the kernel resolver, which would
bypass the `sysinfod` broker's per-principal scoping (a §2 non-goal). That
userspace resolver is landed for the selectors the shipped queries back
(`info:system/{hostname,kernel,machine-id}`, `stats:uptime`,
`stats:mem/{used,available,total}`); the kernel resolver fails `info:`/`stats:`
closed. The remaining breadth (more sysinfo-backed selectors; the device
namespaces via the device manager) and the richer resolver error model (§19)
are added in place as their consumers appear. See `plans/SHELL.md`
P5.

This document specifies TAIRiX resource aliases and selector namespaces. It is
intended to be used together with the TAIRiX drive/path conventions document.
The companion file owns filesystem path spelling, drive mount naming, and any
exact persistent storage path. This file owns non-filesystem resource references
such as `sys:random`, `disk:backup`, `info:cpu/vendor`, and
`stats:net/wan/rx.pps`.

This document does not weaken AGENTS.md. Where this document and AGENTS.md
conflict, AGENTS.md wins.

## 1. Purpose

TAIRiX has no `/dev`, no `/proc`, and no `/sys`. Resource references therefore
must not be implemented as a pseudo-filesystem and must not create filesystem
entries that pretend devices, statistics, or kernel state are files.

The goal is to provide a short, human-usable naming system that still gives the
implementation enough information to resolve resources safely:

- names are typed by namespace;
- user aliases are not durable identity;
- durable identity is checked before dangerous operations;
- resource resolution returns a capability, not a pathname;
- live information is obtained through typed APIs, not text scraping;
- shell completion participates in safety by showing identity and inserting
  guards where required.

## 2. Non-goals

The following are forbidden by this specification:

- a `/dev` replacement implemented as a virtual filesystem;
- a `/proc` or `/sys` compatibility tree;
- discovery-order names such as `disk:0`, `disk:a`, `dev:sda`, or `disk:sda` as
  stable scriptable names;
- a generic untyped `ioctl`-style control plane;
- accepting self-reported labels as trusted identity;
- resolving a destructive target through an unguarded alias in non-interactive
  execution;
- bypassing the System Information API for `info:` or `stats:` data;
- bypassing device-manager and hardware-tree discovery for device references.

## 3. Terms

### 3.1 Resource reference

A resource reference is a string accepted by the shell and ABI helpers that
selects a typed non-filesystem resource.

Examples:

```text
sys:random
info:cpu/vendor
stats:net/wan/rx.pps?window=1s
disk:backup@7K2M
disk:slot/front-usb@P91Q::raw
tty:debug
vol:home
```

A resource reference is not a file path.

### 3.2 Namespace

The prefix before `:`. The namespace defines the resource class and the resolver
that owns the selector. Examples: `sys`, `disk`, `vol`, `tty`, `info`, `stats`.

### 3.3 Selector

The namespace-specific text after `:` and before any identity guard, facet, or
query parameters.

In `disk:backup@7K2M::raw`, the selector is `backup`.

### 3.4 Alias

A human-friendly local name bound to a resource identity. An alias is never the
identity itself.

Examples:

```text
disk:backup
vol:home
tty:debug
net:wan
```

### 3.5 Pinned alias

A persistent alias that records the expected resource kind, canonical identity,
short fingerprint, and selected expected properties. Pinned aliases fail closed
if the matching resource is not present or no longer matches.

### 3.6 Identity guard

A short fingerprint appended with `@` to require that the resolved resource
matches a specific identity.

Examples:

```text
disk:backup@7K2M
disk:slot/front-usb@P91Q
vol:home@H41V
```

The guard is a check, not a separate resource name.

### 3.7 Canonical identity

A structured identity record produced by the resource owner. It may contain a
WWN, serial number, hardware-tree node id, filesystem UUID, partition UUID,
LUKS-equivalent container UUID, network interface permanent address, driver
match keys, capacity, model, and other stable properties. The exact fields are
resource-kind specific.

The canonical identity is machine-readable. Humans should not normally type it.

### 3.8 Short fingerprint

A compact display digest derived from the canonical identity and resource kind.
It is used as an identity guard. It is not itself the full identity.

The resolver must require enough characters to make the fingerprint unique among
currently known resources of that kind and among pinned aliases in scope.

### 3.9 Facet

A typed interface exposed by a resource. Facets prevent fake-file behavior and
replace untyped control calls.

Examples:

```text
sys:random::bytes
disk:backup::raw
stats:net/wan::metrics
info:cpu::record
```

### 3.10 Resolve intent

The caller-supplied purpose for resolution. The resolver must receive the intent
before returning a handle or answer.

Required intents:

```text
MetadataRead
Read
Write
Mount
Configure
ObserveStats
ObserveInfo
RawRead
RawWrite
DestructiveWrite
ExclusiveDestructiveWrite
```

The resolver must reject a resource if the selector is valid but unsafe for the
requested intent.

### 3.11 Resource capability

The result of successful resolution is a typed capability or typed response, not
a pathname. Holding a capability is what grants access. A string reference alone
grants nothing.

## 4. Core model

The resource alias system has four layers:

```text
text reference -> parsed selector -> resolver request -> typed capability/response
```

Resolution order:

1. Parse the reference.
2. Identify the namespace.
3. Determine the required resource kind, facet, and resolve intent.
4. Check the caller identity and capability set before reading or mutating any
   protected state.
5. Resolve aliases, direct identities, topology selectors, or query selectors.
6. Check any identity guard.
7. Check generation and hotplug state where applicable.
8. Return a typed capability or typed response.
9. Log security-relevant allow/deny decisions with a stable event id.

The resolver must fail closed at every step.

## 5. Syntax

### 5.1 Grammar

Resource references use this grammar:

```text
resource-ref   = namespace ":" selector [ guard ] [ facet ] [ params ]
namespace      = ident
selector       = selector-part *( "/" selector-part )
guard          = "@" fingerprint
facet          = "::" ident
params         = "?" param *( "," param )
param          = key op value
op             = "=" | "!=" | "<" | "<=" | ">" | ">=" | "~"
```

### 5.2 Character rules

Namespaces, aliases, facets, and parameter keys are lowercase ASCII:

```text
a-z 0-9 - _
```

Rules:

- namespace names must start with a letter;
- aliases must start with a letter or digit;
- aliases must not contain whitespace;
- aliases must not contain `/`, `:`, `@`, `?`, `,`, or `::`;
- resource references are case-sensitive;
- user-facing aliases should use lowercase kebab-case, e.g. `backup-disk`;
- canonical fields may use resource-specific encoding but must be escaped when
  rendered inside a resource reference.

### 5.3 Reserved characters

```text
:   separates namespace from selector
/   separates selector path segments
@   introduces identity guard
::  introduces facet
?   introduces parameters
,   separates parameters
```

### 5.4 Direct identity shorthand

A selector beginning with `@` selects by short fingerprint within the namespace.

Examples:

```text
disk:@7K2M
vol:@H41V
tty:@L44R
```

The shorthand must resolve uniquely. If it does not, the resolver returns
`AmbiguousSelector` and reports the minimum longer fingerprint required.

### 5.5 Full identity selectors

Each namespace may define full identity selectors. Full identity selectors are
machine-oriented and should be generated by tools, not typed by users in normal
use.

Examples:

```text
disk:id/wwn/5002538f00000000
disk:id/serial/S6XYZ123456789
part:id/partuuid/8f4d2c01-0001
vol:id/fsuuid/b6f1e6d9-42a7-4c0e-8b26-0ac61f2d91b7
```

Full identity selectors are still subject to capability checks.

## 6. Namespace registry

The following namespaces are defined by this specification.

### 6.1 `sys:`

`sys:` names system services and abstract kernel-backed resources. It is not a
filesystem path and is not `/sys`.

Required entries:

```text
sys:random              cryptographic random byte source
sys:clock/monotonic     monotonic clock source
sys:clock/wall          wall-clock source, read subject to policy
sys:entropy             entropy status and privileged entropy operations
sys:log                 structured log service handle, capability-gated
sys:capability          capability inspection and delegation service handle
```

`sys:random` maps to the canonical random ABI. It must not expose hardware RNG
bytes directly. It must expose random bytes only through the TAIRiX random
subsystem.

`sys:entropy` is not a writeable byte sink by default. Entropy injection, if
provided, is a typed privileged operation requiring an explicit capability.

### 6.2 `info:`

`info:` names mostly stable facts about the system and resources.

It answers: what is this thing, what can it do, where is it, and what is it
made of?

Examples:

```text
info:system
info:system/kernel
info:cpu/vendor
info:cpu/model
info:cpu/features
info:cpu/topology
info:cpu/cache
info:cpu/microcode
info:cpu/vulnerabilities
info:mem/physical
info:mem/numa
info:disk/backup/model
info:disk/backup/serial
info:disk/backup/capacity
info:disk/backup/block-size
info:net/wan/driver
info:net/wan/mac
info:net/wan/capabilities
info:tty/debug/driver
```

`info:` must be backed by the System Information API. It must not be served by a
virtual file or by text scraping.

`info:` is therefore **value-backed**, not a byte stream: it is read as a typed
value through the broker, never opened as a kernel descriptor. That is a
structural property, not a resolver that has yet to land, and every layer
states it the same way. The registry classifies the namespace
(`tairix_resref::NamespaceBacking::Value`); the kernel resource resolver
refuses such a reference with `ResolveError::NotAStream` →
`Errno::NotSupported` ("the subsystem is live, this backing cannot represent
the request, and retrying can never succeed"), deliberately distinct from the
`Errno::NotImplemented` a *stream* namespace with no resolver wired yet
returns.

**A value is still readable as bytes — by a reader that goes through the
broker.** The kernel cannot serve one; userspace can, in three spellings that
resolve through the one resolver (`lib/procinfo`) and render with the one
`display_value`, so their bytes are identical:

- `sysinfo show <ref>` — the value printed (§15.4).
- `cat <ref>` — the reference as a tool's **operand**, resolved by the tool
  itself through the shared `NamedSource`. The reading process is the tool, so
  the tool's own manifest carries the `CAP_SYSINFO_*` request.
- `cat < <ref>` — a **read redirection**, resolved by the shell, which hands
  the child a filled pipe. The child reads an ordinary descriptor and needs no
  `CAP_SYSINFO_*`, so every stdin-consuming tool gains the read at once
  without each being widened.

None bypasses the broker: the authority is always `manifest ∩ account
ceiling`, checked at `sysinfod` against the reading process's attested set, so
an account lacking the query's capability is refused with that capability
named.

Reading is the whole of it. A value-backed reference is still never a *write*
target: `>`, `>>`, and `<>` keep reaching the kernel resolver and keep failing
with `Errno::NotSupported`, because such a resource is changed by a typed
service command, not by writing text at it (§6.4). Shell completion follows
the same split — value-backed namespaces are offered after `<` and never after
a writing operator (§15.3). The direct shell-facing way to read one, with no
redirection, is `show` (§15.4).

`info:` values may be sensitive. The resolver must not assume that information
is public. Examples requiring policy review include hardware serial numbers,
MAC addresses, machine identity, full hardware topology, and system-wide process
or memory details.

### 6.3 `stats:`

`stats:` names time-dependent measurements.

It answers: how much, how fast, how busy, and how many since a defined origin?

Examples:

```text
stats:cpu/load
stats:cpu/load.1m
stats:cpu/load.5m
stats:cpu/core/0/usage
stats:cpu/core/0/frequency
stats:cpu/package/0/temp
stats:mem/used
stats:mem/available
stats:mem/pagefaults
stats:net/wan/rx.bytes
stats:net/wan/rx.packets
stats:net/wan/rx.pps?window=1s
stats:net/wan/rx.bps?window=10s
stats:disk/backup/read.bytes
stats:disk/backup/write.bytes
stats:disk/backup/read.iops?window=1s
stats:disk/backup/queue.depth
```

`stats:` must be backed by the System Information API. It must not be served by
a virtual file or by text scraping. Like `info:` it is value-backed: no kernel
descriptor can be opened on it, and the shell's read redirection is how it is
consumed as bytes (§6.2).

Metric values must carry metadata:

```text
kind: counter | gauge | rate | derived-rate | histogram
unit: bytes | bits/s | packets | packets/s | percent | celsius | ns | count
source: resource reference or canonical resource id
time: Time64 or monotonic timestamp, as appropriate
window: required for rates and derived rates
reset_behavior: boot | resource-reset | driver-reset | never | declared
```

Rates are undefined without a sampling window. A default display window may be
used for interactive shell rendering, but scripts must be able to specify the
window explicitly.

### 6.4 `state:`

`state:` names current mutable state or configuration.

It answers: what mode is this thing in, what policy is active, and what current
configuration is selected?

Examples:

```text
state:security/mitigations
state:security/mitigations/spectre-v2
state:cpu/governor
state:net/wan/link
state:net/wan/address
state:disk/backup/mounts
state:audio/sink/default
```

`state:` is intentionally separate from `info:` and `stats:`. CPU vulnerability
facts belong under `info:cpu/vulnerabilities`; active mitigation policy belongs
under `state:security/mitigations`.

`state:` reads must be capability-checked. State changes must be performed by
typed service commands, not by writing text into a pseudo-file. `state:` is
value-backed on the read side too — no kernel descriptor opens on it, and a
read is served through the broker (§6.2). The write side is why that
asymmetry matters: a *write* is a typed command, never a redirection, so
`> state:…` stays refused even though `< state:…` reads.

### 6.5 `disk:`

`disk:` names whole block storage devices.

Allowed selector forms:

```text
disk:<alias>
disk:<alias>@<fingerprint>
disk:@<fingerprint>
disk:id/<identity-kind>/<value>
disk:slot/<topology-name>
disk:slot/<topology-name>@<fingerprint>
disk:?<query>
```

Examples:

```text
disk:system
disk:backup
disk:backup@7K2M
disk:@7K2M
disk:id/wwn/5002538f00000000
disk:slot/front-usb
disk:slot/front-usb@P91Q
disk:?removable=true,size>=16GiB
```

Rules:

- `disk:` means a whole device, not a partition and not a filesystem volume.
- Discovery-order names are forbidden as persistent aliases.
- Self-reported disk labels are weak selectors and must not become trusted
  aliases automatically.
- Raw access requires the `raw` facet.
- `RawWrite`, `DestructiveWrite`, and `ExclusiveDestructiveWrite` require an
  identity guard in non-interactive execution.
- Removable media must include generation checks so that a handle becomes stale
  after media removal or replacement.

### 6.6 `part:`

`part:` names partitions or slices.

Allowed selector forms:

```text
part:<disk-alias>/<index>
part:<alias>
part:<alias>@<fingerprint>
part:@<fingerprint>
part:id/partuuid/<value>
part:?<query>
```

Examples:

```text
part:backup/1
part:efi
part:efi@F22D
part:id/partuuid/8f4d2c01-0001
```

Rules:

- `part:` must never resolve to a whole disk.
- A partition alias must record its parent disk identity and partition identity.
- If the parent disk matches but the partition table generation changed, the
  alias must fail with `StaleGeneration` unless re-pinned.

### 6.7 `vol:`

`vol:` names formatted filesystem volumes.

Allowed selector forms:

```text
vol:<alias>
vol:<alias>@<fingerprint>
vol:@<fingerprint>
vol:id/fsuuid/<value>
vol:label/<self-reported-label>
vol:?<query>
```

Examples:

```text
vol:home
vol:backup
vol:backup@H41V
vol:id/fsuuid/b6f1e6d9-42a7-4c0e-8b26-0ac61f2d91b7
vol:label/Backup
```

Rules:

- `vol:` means a filesystem volume, not a whole disk.
- `vol:label/...` is a weak selector because labels are self-reported.
- Mount operations must use `Mount` intent and must pass the mount policy.
- A volume alias must record the volume identity and, where available, expected
  parent partition or container identity.

### 6.8 `tty:`

`tty:` names terminal and serial resources.

Examples:

```text
tty:console
tty:debug
tty:debug@L44R
tty:@L44R
tty:?usbvid=0403,pid=6001
```

Rules:

- Serial configuration is a typed `tty` operation, not an ioctl.
- A `tty:` resource may expose `ByteStream` and `SerialConfig` facets.
- Baud rate, parity, stop bits, modem control, and flow control must be typed
  operations on a `tty` capability.
- Disk operations must reject `tty:` resources with `WrongKind`.

### 6.9 `net:`

`net:` names network interfaces and network endpoints.

Examples:

```text
net:wan
net:lan
net:wifi
net:loopback
net:@N82C
net:?kind=ethernet,link=up
```

Rules:

- `net:` aliases are pinned to interface identity, not driver load order.
- MAC addresses may be privileged information when exposed through `info:`.
- Packet capture, raw socket, or address configuration requires explicit
  capabilities.

### 6.10 `input:`

`input:` names input devices or logical input groups.

Examples:

```text
input:keyboard/main
input:pointer/main
input:mouse/external
input:gamepad/primary
```

Rules:

- Applications must not receive ambient access to all input devices.
- Session services may mediate input capabilities to applications.
- Device-specific selectors must resolve through the hardware tree and driver
  manager.

### 6.11 `audio:`

`audio:` names audio sinks, sources, and policy-selected defaults.

Examples:

```text
audio:sink/default
audio:sink/speakers
audio:source/microphone
audio:source/default
```

Rules:

- Microphone access is capability-gated.
- `audio:sink/default` and `audio:source/default` are policy selectors, not
  stable hardware identities. Dangerous or persistent configuration must pin
  the underlying identity where applicable.

### 6.12 `gpu:`

`gpu:` names graphics and compute-capable display resources.

Examples:

```text
gpu:display/main
gpu:framebuffer/boot
gpu:compositor/default
```

Rules:

- The graphical desktop is optional. Absence of a matching `gpu:` resource must
  be handled by text login and headless operation.
- Access to framebuffer or compositor resources is through public capabilities
  only.

### 6.13 `bus:`

`bus:` names buses and bus nodes exposed by the hardware tree.

Examples:

```text
bus:pci/root
bus:usb/main
bus:virtio/main
```

Rules:

- `bus:` is primarily for privileged diagnostics, driver management, and
  hardware inventory.
- `bus:` must not expose raw MMIO or DMA handles without explicit capabilities.

### 6.14 `svc:`

`svc:` names long-running system services.

Examples:

```text
svc:init
svc:devmgr
svc:sysinfo
svc:login
svc:network
svc:resolver
```

Rules:

- `svc:` is not a process-id alias. It names service identity.
- Service control is capability-gated.
- Service status should be visible through `state:svc/...` and service metrics
  through `stats:svc/...`.

### 6.15 `proc:`

`proc:` names process handles exposed by capability-checked process-management
APIs. It is not `/proc`.

Examples:

```text
proc:self
proc:child/<spawn-id>
proc:pid/<pid>
```

Rules:

- `proc:self` is available to the caller.
- `proc:pid/...` may be privileged for processes not owned by the caller.
- Process information belongs under `info:proc/...`, `stats:proc/...`, and
  `state:proc/...` via the System Information API, not through a file tree.

### 6.16 `cap:`

`cap:` is reserved for shell-internal display of already-held capabilities.

Rules:

- `cap:` references must not serialize authority into copyable text.
- A printed `cap:` value is descriptive unless explicitly represented by an
  unforgeable handle in the shell runtime.
- Secrets and capability tokens must never be written to stdout, stderr,
  stdinfo, logs, or history.

## 7. Alias records

A pinned alias record contains at least:

```text
version
namespace
alias
scope
resource_kind
canonical_identity
short_fingerprint
expected_properties
created_by
created_at
last_confirmed_at
policy
```

`expected_properties` should include stable, user-meaningful checks where
available:

```text
model
serial
wwn
capacity
removable
parent_identity
filesystem_type
volume_uuid
partition_uuid
hardware_tree_node_id
```

Alias records must not contain secrets or live capability tokens.

## 8. Alias scopes

Required scopes:

```text
machine    available to system services and all users subject to permissions
user       available to one user session
session    exists only for the current login/session
command    exists only for the current shell command or script block
```

Resolution rules:

- System services must resolve machine aliases unless explicitly handed a
  user-mediated capability.
- Interactive user shells may resolve user aliases before machine aliases, but
  destructive operations still require identity guards.
- Session and command aliases must never be persisted silently.
- A lower-trust scope must not silently override a higher-trust alias for a
  privileged operation.

The companion path conventions file defines exact storage locations. Machine
aliases must live under machine-wide settings. User aliases must live under the
user settings hierarchy. No alias database may be stored under a new top-level
filesystem directory.

## 9. Pinning and re-pinning

### 9.1 Pinning

Pinning creates a local alias for a discovered resource.

Example shell flow:

```text
disk list
disk pin @7K2M as backup
```

The command must show a resource card before committing the alias:

```text
@7K2M  disk candidate
  kind: disk
  model: Samsung SSD 870 EVO
  serial: S6XYZ...
  capacity: 4 TiB
  removable: false
  current topology: sata/2
```

Pinning must fail if the selected fingerprint is ambiguous, stale, or not
present.

### 9.2 Re-pinning

Re-pinning changes an alias to a different identity.

Example:

```text
disk repin backup @9A4F
```

Rules:

- Re-pinning requires explicit capability according to namespace and scope.
- Re-pinning must log the old and new canonical identity digests.
- Re-pinning must not happen automatically because a similar device appeared.
- Re-pinning must not be performed by self-reported label alone.

### 9.3 Alias failure

If an alias does not match its pinned identity, it fails closed.

Required diagnostic shape:

```text
Refusing: disk:backup currently resolves to @9A4F, not @7K2M.

Expected:
  disk:backup@7K2M
  model: Samsung SSD 870 EVO
  capacity: 4 TiB

Found:
  disk:@9A4F
  model: Crucial MX500
  capacity: 2 TiB
```

## 10. Identity fingerprints

### 10.1 Derivation

The fingerprint input is:

```text
resource_kind || canonical_identity || identity_version
```

The display encoding should avoid ambiguous characters. Crockford-style base32
or another documented unambiguous uppercase alphabet is acceptable.

### 10.2 Length

Default display length: 4 characters.

The resolver must request a longer prefix when needed for uniqueness.

Examples:

```text
@7K2M
@7K2M-Q8
@7K2M-Q8D3
```

The dash is display sugar and must be ignored by fingerprint comparison.

### 10.3 Collision behavior

Collision handling is fail-closed:

- if the supplied fingerprint prefix is ambiguous, return `AmbiguousSelector`;
- if an alias guard does not match the resolved canonical identity, return
  `IdentityMismatch`;
- if the canonical identity changed because of hotplug or media replacement,
  return `StaleGeneration` or `IdentityMismatch` according to the resource kind.

## 11. Weak selectors

The following selectors are weak by default:

```text
vol:label/<label>
disk:label/<label>
disk:slot/<topology>
net:?name=<driver-reported-name>
audio:sink/default
audio:source/default
```

Weak selectors may be used for observation and interactive selection. They must
not be accepted for `RawWrite`, `DestructiveWrite`, or
`ExclusiveDestructiveWrite` unless combined with an identity guard.

Example accepted:

```text
image-write os.iso -> disk:slot/front-usb@P91Q::raw
```

Example rejected:

```text
image-write os.iso -> disk:slot/front-usb::raw
```

Required diagnostic:

```text
Refusing destructive write through weak selector disk:slot/front-usb.
Current match is disk:slot/front-usb@P91Q.
Use the guarded form after checking the displayed identity.
```

## 12. Resolve intents and guard policy

Required guard policy:

```text
Intent                      Unguarded alias  Weak selector  Guard required
MetadataRead                allowed          allowed        no
ObserveInfo                 allowed          allowed        no
ObserveStats                allowed          allowed        no
Read                        allowed          allowed        no
Mount                       allowed          discouraged    maybe by policy
Configure                   allowed          maybe          maybe by policy
RawRead                     allowed          maybe          maybe by policy
RawWrite                    denied in script denied         yes
DestructiveWrite            denied           denied         yes
ExclusiveDestructiveWrite   denied           denied         yes
```

Interactive shells may offer a confirmation flow for unguarded destructive
commands, but the actual operation must pass an identity guard to the resolver.
Scripts and non-interactive commands must not be prompted; they must fail with
`UnsafeUnguardedSelector`.

## 13. Facets and typed operations

Required facets:

```text
ByteSource          readable byte source
ByteSink            writable byte sink
ByteStream          bidirectional byte stream
BlockDevice         block read/write with sector semantics
RawBlock            privileged raw block facet
FilesystemVolume    mountable filesystem volume
InfoRecord          structured information response
Metric              single metric response
MetricGroup         grouped metrics response
ServiceControl      service-management operations
SerialConfig        serial-port configuration operations
NetworkInterface    network configuration and observation operations
InputEventSource    input event stream
AudioSink           audio output stream/control
AudioSource         audio input stream/control
```

Rules:

- A resource exposes only the facets it actually supports.
- Control operations are typed methods on the appropriate facet.
- A disk must not accept serial operations.
- A serial port must not accept disk operations.
- Unsupported operations return `UnsupportedFacet` or `WrongKind`, not an
  untyped device-specific error.

Examples:

```text
head -c 32 < sys:random
copy image.bin -> disk:installer@P91Q::raw
tty set tty:debug baud=115200 parity=none
show info:cpu/vendor
watch stats:net/wan/rx.pps?window=1s
```

`cat disk:backup` should fail unless an explicit byte or raw facet is selected.

Required diagnostic:

```text
disk:backup is a BlockDevice, not a default byte stream.
For raw block access use disk:backup@7K2M::raw with the required capability.
```

## 14. `info:` and `stats:` data model

### 14.1 Shared response envelope

`info:` and `stats:` responses must use structured ABI records.

Required envelope fields:

```text
version
query
producer
timestamp
authorization
payload
```

`timestamp` must use the TAIRiX canonical time representation.

This envelope is realized as `lib/procinfo::resinfo::ResourceResponse`
(`version`/`producer`/`authorization`/`timestamp`/`query` + a typed
`ResponsePayload`), produced by the userspace resolver
(`lib/procinfo::resolve`) from the System Information API's replies. The
`query` field doubles as the payload `source` (§14.2/§14.3), so the source is
not stored twice. It is a userspace record built and consumed in-process
today, so it carries no wire encoding; a wire form is added only if a response
crosses a boundary (`version` exists so that shape can be negotiated then).
The variant sets (`MetricKind`, `Unit`, `ResetBehavior`, `Sensitivity`,
`ValueKind`, `Producer`) are closed to exactly what a resolver produces, and
grow in place as producers appear.

### 14.2 `info:` payloads

`info:` payloads are records, not free-form text.

Example conceptual payload:

```text
query: info:cpu/vendor
payload:
  kind: string
  value: GenuineIntel
  source: sysinfo
  sensitivity: public
```

### 14.3 `stats:` payloads

`stats:` payloads are metric values or metric groups.

Required metric fields:

```text
name
kind
unit
value
source
sample_time
window
reset_behavior
```

Counters must identify their reset behavior. Rates must identify their window.

### 14.4 Snapshots

The System Information API must support coherent snapshots for callers that
need multiple metrics from the same observation point.

Example shell form:

```text
stats snapshot cpu/load net/wan/rx.bytes net/wan/tx.bytes disk/backup/write.bytes
```

The snapshot must include:

```text
snapshot_id
monotonic_time
wall_time
boot_id
values
```

### 14.5 Metric description

Every metric must be describable.

Example:

```text
describe stats:net/wan/rx.pps
```

Required description fields:

```text
name
kind
unit
source
default_window
valid_windows
capability_required
reset_behavior
```

## 15. Shell behavior

### 15.1 Completion

Shell completion must be namespace-aware and command-intent-aware.

The candidate source is the namespace registry's **selector catalogue**: the
selectors the platform serves today, held once beside the namespace registry
itself (`lib/resref`) and cross-checked by each serving resolver's own tests —
the kernel's resource resolver for `sys:`, the userspace System Information
resolver (`lib/procinfo`) for `info:`, `state:`, and `stats:`. Completion
therefore cannot advertise a selector nothing serves, and a namespace with no
resolver wired offers nothing rather than a plausible-looking set.

A catalogue entry may spell a segment `<name>`: a placeholder for a name
discovered per machine (an interface, an interrupt line) or defined by a closed
table elsewhere (a resource-limit kind, a reclaim class). Each spelling names a
typed *selector domain*. Completion must expand a placeholder into that
domain's real names and offer those; it must never display the placeholder
spelling, which no shell could insert.

Expansion is capability-adaptive, and that is the requirement rather than a
fallback: a domain from a closed table or an ungated query lists for every
session, while a gated one (an interface list costs `CAP_SYSINFO_HW`, a bond
alias `CAP_SYSINFO_GLOBAL`) lists only for a session holding that capability
and offers nothing otherwise — a session that cannot list interfaces could not
read an interface's facts either. A gated domain the session lacks must be
skipped without issuing the query, so completion never produces a denied
request or an audit refusal record. The *catalogue* is never filtered this way:
discovery is not authorization and a spelling grants nothing (§6.2), so a
selector the session cannot read is still offered and the read then fails
naming the capability (`plans/SHELL.md`, "Tab expansion and completion").

For safe commands, completion may insert short aliases:

```text
disk info disk:backup
```

For destructive commands, completion must insert guarded references:

```text
format disk:backup@7K2M
```

Completion display should show resource cards:

```text
disk:backup@7K2M      Samsung SSD 870 EVO   4 TiB   pinned, non-removable
disk:installer@P91Q   SanDisk Ultra USB     32 GiB  removable, empty
```

Completion must not hide identity mismatches.

### 15.2 Typed shell values

If the shell supports variables, resource selections should be typed values, not
plain strings.

Example:

```text
let target = pick disk:?removable=true,size>=16GiB
image-write installer.img -> $target::raw
```

The variable should carry:

```text
resource_kind
canonical_identity
short_fingerprint
facet_rights
generation
scope
```

Serializing the variable to text must not serialize authority.

### 15.3 Redirection

Redirection is allowed only for resources with stream facets.

Examples:

```text
head -c 64 < sys:random
tty monitor tty:debug > log.txt
cmd 3>info.jsonl
```

Rules:

- `stdin`, `stdout`, `stderr`, and `stdinfo` remain descriptor-based.
- A text program must not bind itself directly to a discovered device.
- Resource references used in redirection resolve to stream backings through the
  stream layer.
- A **value-backed** namespace (`info:`, `state:`, `stats:` — §6.2) has no
  stream facet, so no kernel descriptor is ever opened on one: the resolver
  refuses every direction with `Errno::NotSupported`.
- It can nonetheless be a redirection **source**. `cmd < info:mem/physical`
  is served by the shell, not the kernel: it reads the value through the
  broker under its own attested identity and hands the child a pipe. The
  child sees an ordinary descriptor and needs no `CAP_SYSINFO_*`.
- It can never be a redirection **target**. `>`, `>>`, `<>`, and `&>` on a
  value-backed reference reach the kernel resolver and are refused, because
  such a resource is changed by a typed service command (§6.4). `<>` is
  refused despite its read half: serving half a request would silently
  downgrade it.
- Completion follows exactly that split — value-backed namespaces and their
  selectors are offered after a reading operator and never after a writing
  one, as a namespace prefix or as a selector.

### 15.4 Standard commands

Required shell-facing commands:

```text
show <resource-ref>
describe <resource-ref>
watch <stats-ref>
resolve <resource-ref>
pin <resource-ref> as <alias>
unpin <namespace:alias>
```

Realised today, for the value-backed namespaces (`info:`, `state:`, `stats:`):

```text
sysinfo show <resource-ref>        # the value
sysinfo describe <resource-ref>    # the envelope: producer, authorization,
                                   # sensitivity, and a metric's
                                   # kind/unit/reset-behaviour/window (§14.5)
cmd < <resource-ref>               # the value as the command's standard input
```

All three resolve through the one userspace resolver (`lib/procinfo::resolve`)
over the System Information API, so none is a second reader and none can
bypass the broker's per-principal scoping. A denial names the capability the
query declares, read from the frozen `sysinfo-v1` registry, so the user learns
which grant to ask for.

The two subcommands live in the existing `sysinfo` tool rather than a new
bundle: it already holds exactly `CAP_SYSINFO_GLOBAL|KERNEL|HW` and already
links the resolver, so minting a second bundle with identical privilege would
add attack surface and buy nothing. The redirection form is the shell's,
because the reading process has to be the one holding the authority and a
redirection's child is chosen by the user — so the shell's own manifest
carries the same trio (§15.3). That widens one long-lived process rather than
every reader tool, and adds no reach: holding `CAP_PROC_SPAWN`, the shell
could already read any of these facts out of a spawned `sysinfo`. Making the
read direct makes it attributable to the shell instead of laundered through a
child.

`watch <stats-ref>`, `pin`, and `unpin` are not yet built: `watch` needs a
sampling cadence and a display loop, and `pin`/`unpin` need the alias-record
subsystem of §7–§9, which does not exist yet. `resolve <resource-ref>` reports
a canonical identity and short fingerprint (§10), which only the
device namespaces carry — a value-backed reference has neither — so it lands
with the first device-namespace resolver.

Namespace-specific commands may wrap these:

```text
disk list
disk info <disk-ref>
disk pin <disk-ref> as <alias>
disk repin <alias> <disk-ref>
disk format <disk-ref>
tty list
tty set <tty-ref> baud=<n> parity=<mode>
stats get <stats-ref>
stats snapshot <stats-ref>...
```

## 16. Service and ABI placement

### 16.1 Shared parsing

Resource-reference parsing must have one shared implementation used by the
shell, system services, tests, and any ABI helper that accepts resource
references. The parser belongs in shared Rust code under `lib/` if more than
one crate needs it.

### 16.2 ABI types

If a resource reference crosses a syscall, IPC, or service boundary, the ABI
must use structured types, not raw strings alone.

Required conceptual ABI types:

```text
ResourceRef
ResourceNamespace
ResourceSelector
ResourceFacet
ResolveIntent
IdentityGuard
IdentityFingerprint
AliasScope
AliasRecord
ResourceKind
ResourceDescriptor
MetricQuery
InfoQuery
```

A textual reference may be accepted at the shell boundary, but service and
kernel boundaries should receive parsed structured requests.

### 16.3 Resolver ownership

The resource resolver may be implemented as a service under `/System/Services/`
or as shared ABI helpers plus namespace-specific services. In either case:

- `info:` and `stats:` queries are served by the System Information API;
- hardware inventory comes from the hardware tree;
- device binding comes from device-manager and driver manifests;
- storage aliases must not maintain a parallel hardware inventory;
- all authorization decisions must use kernel-provided caller identity.

### 16.4 Logging

Security-relevant decisions must be logged through structured logging with
stable event ids.

Examples:

```text
alias.resolve.denied.capability
alias.resolve.denied.identity_mismatch
alias.resolve.denied.unsafe_unguarded_selector
alias.pin.changed
alias.repin.changed
alias.unpin.changed
```

Do not use `stdinfo` for audit or security logs.

## 17. Query selectors

Query selectors begin with `?` and must resolve to exactly one resource unless
used by an explicitly list-oriented command.

Examples:

```text
disk:?removable=true,size>=16GiB
net:?kind=ethernet,link=up
tty:?usbvid=0403,pid=6001
```

Rules:

- Query keys are namespace-specific and versioned.
- Unknown query keys return `UnknownQueryKey`.
- A query that matches zero resources returns `NotFound`.
- A query that matches more than one resource returns `AmbiguousSelector`.
- List commands may request all matches explicitly.
- Dangerous operations require an identity guard even when the query resolves to
  one resource at that moment.

## 18. Topology selectors

Topology selectors name physical or logical attachment points.

Examples:

```text
disk:slot/front-usb
bus:usb/main
net:port/rear-1
```

Rules:

- Topology selectors are useful for interactive workflows.
- Topology selectors are not durable identity.
- Destructive operations through topology selectors require identity guards.
- Topology names must come from hardware discovery or administrator policy,
  not discovery order.

## 19. Error model

Required resolver errors:

```text
InvalidSyntax
UnknownNamespace
UnknownAlias
UnknownQueryKey
UnsupportedFacet
WrongKind
CapabilityDenied
NotFound
AmbiguousSelector
IdentityMismatch
StaleGeneration
UnsafeUnguardedSelector
WeakSelectorRequiresGuard
ResourceRemoved
ResourceBusy
ExclusiveAccessDenied
MetricWindowRequired
MetricWindowOutOfRange
InfoQueryDenied
StatsQueryDenied
```

Errors must be typed. Human-readable text is a rendering of the typed error,
not the primary programmatic interface.

## 20. Examples

### 20.1 Pin and use a backup disk

```text
disk list
disk pin @7K2M as backup
backup run --target disk:backup
disk health disk:backup
```

Destructive operation:

```text
format disk:backup
```

Non-interactive result:

```text
UnsafeUnguardedSelector: destructive operation requires disk:backup@7K2M
```

Accepted:

```text
format disk:backup@7K2M
```

### 20.2 Write an installer image to USB

```text
disk list removable
image-write tairix.img -> disk:slot/front-usb@P91Q::raw
```

This means: use the device currently in the front USB slot only if it is still
the device whose identity fingerprint is `P91Q`, and use the privileged raw
block facet.

### 20.3 Read random bytes

```text
head -c 32 < sys:random
```

This resolves `sys:random` to a cryptographic random byte source through the
random ABI.

### 20.4 CPU information and mitigations

```text
show info:cpu/vendor
show info:cpu/model
show info:cpu/vulnerabilities
show state:security/mitigations
```

Vulnerability facts are `info:`. Active mitigation configuration is `state:`.

### 20.5 Network rates

```text
show stats:net/wan/rx.pps?window=1s
watch stats:net/wan/rx.bps?window=10s
```

The metric result must include units, timestamp, source, and window.

### 20.6 Serial debug port

```text
tty list
tty pin @L44R as debug
tty set tty:debug baud=115200 parity=none
tty monitor tty:debug
```

Baud rate is a typed TTY operation. It must not be accepted by a disk.

## 21. Security rules

The resolver and all namespace services must obey these rules:

1. Identify the caller from kernel-provided identity, not caller input.
2. Check capabilities before reading or mutating protected state.
3. Validate every field of every request.
4. Fail closed on absent resources, absent capabilities, stale generations,
   ambiguity, unsupported facets, and identity mismatches.
5. Return typed errors, not stringly typed failures.
6. Log security-relevant allow and deny decisions.
7. Never expose capability tokens, secrets, or raw handles in stdout, stderr,
   stdinfo, logs, shell history, aliases, or config files.
8. Treat self-reported labels and driver-reported names as untrusted display
   data unless pinned by local policy.
9. Do not grant authority from namespace spelling alone.
10. Do not create new ambient authority for root or system users.

## 22. Implementation checklist for AI agents

When implementing this specification, an AI agent must satisfy all of these:

- use Rust only;
- use one shared parser implementation;
- add ABI types in the ABI source of truth when resource references cross an
  ABI boundary;
- update docs in the same change;
- add negative tests for every fail-closed path;
- add parser tests for valid and invalid syntax;
- add resolver tests for aliases, direct ids, topology selectors, query
  selectors, identity guards, stale generations, and collisions;
- add capability tests proving that checks happen before protected state is
  touched;
- add shell completion tests proving destructive completions insert guards;
- add System Information API tests for `info:` and `stats:` query authorization;
- add hotplug/removal tests proving stale handles are invalidated;
- add tests proving `/dev`, `/proc`, and `/sys` are not created or used;
- update PLAN.md and relevant docs when adding a new shared crate, ABI surface,
  namespace, or service;
- run the full TAIRiX validation gate before reporting an in-repository change
  complete.

## 23. Acceptance tests

A correct implementation must pass tests equivalent to the following.

### 23.1 Parser acceptance

Accepted:

```text
sys:random
info:cpu/vendor
stats:net/wan/rx.pps?window=1s
disk:backup@7K2M
disk:slot/front-usb@P91Q::raw
tty:debug
vol:id/fsuuid/b6f1e6d9-42a7-4c0e-8b26-0ac61f2d91b7
```

Rejected:

```text
/dev/random
/proc/cpuinfo
/sys/class/net
Disk:Backup
disk:back up
disk:backup@@7K2M
disk:backup::
stats:net/wan/rx.pps?window
```

### 23.2 Resolver security

- `format disk:backup` fails in non-interactive mode.
- `format disk:backup@wrong` fails with `IdentityMismatch`.
- `image-write x -> disk:slot/front-usb::raw` fails with
  `WeakSelectorRequiresGuard`.
- `image-write x -> disk:slot/front-usb@P91Q::raw` succeeds only if the current
  resource still matches `P91Q` and the caller has the required capability.
- `tty set disk:backup baud=115200` fails with `WrongKind`.
- `disk info tty:debug` fails with `WrongKind`.
- `show info:disk/backup/serial` fails without the required information
  capability if policy marks serial numbers privileged.
- `watch stats:net/wan/rx.pps` either uses the documented default interactive
  window or fails for scripts with `MetricWindowRequired`.

### 23.3 Hotplug and generation

- A capability opened for a removable device is invalidated when media is
  removed.
- A topology selector guarded by an old fingerprint fails after the device is
  replaced.
- A partition alias fails after the parent partition table generation changes.
- A volume alias fails if a different volume presents the same self-reported
  label.

### 23.4 No pseudo-filesystem regression

- The filesystem layer refuses top-level `/dev`, `/proc`, and `/sys`.
- `info:` and `stats:` are not implemented as filesystem reads.
- Resource aliases are not inode names.
- Device drivers are not discovered by scanning a device file directory.

## 24. Namespace extension rules

Adding a new namespace requires:

- a stated resource kind and owner;
- a typed selector grammar;
- a capability policy;
- a facet list;
- resolver tests;
- shell completion behavior;
- documentation;
- a clear explanation of why an existing namespace is not sufficient.

A new namespace must not be a junk drawer. In particular, `dev:` is reserved and
must not be introduced as a broad catch-all. Device classes should have precise
namespaces such as `disk:`, `tty:`, `net:`, `input:`, `audio:`, `gpu:`, or
`bus:`.

## 25. Summary

TAIRiX resource references are typed selectors that resolve to typed
capabilities or typed information responses. They are not files.

Human-friendly aliases such as `disk:backup` are allowed, but they are not
identity. Dangerous operations require an identity guard such as
`disk:backup@7K2M`. `info:` and `stats:` are shell-facing selector namespaces
backed by the System Information API. Device resources come from the hardware
tree and driver manager. The resolver is capability-checked, fail-closed, and
structured from the shell boundary down to the ABI.
