# SYSLOG.md - TAIRiX System Log Specification

Status: draft, live pre-release specification  
Owner: `lib/log` and `/System/Logs` implementers  
Scope: TAIRiX-native logging, boot log rendering, log search, retention, and audit-log integrity

## Assumptions

- This is a standalone design artifact named `SYSLOG.md` for review. If it is
  imported into the TAIRiX repository, it must either live in an allowed docs or
  plans path, or the repository layout and staged plan must be updated before it
  is accepted.
- TAIRiX has not shipped this log format. The single living definition can be
  edited in place until the first release rather than carrying compatibility
  shims.
- This document specifies behaviour and invariants. It does not implement code,
  add ABI items, or create generated artifacts.

`SYSLOG` in this document means the TAIRiX system log. It is not the Unix syslog
protocol, not a syslog compatibility layer, and not a text-file log format.

## 1. Design goals

TAIRiX logging exists to record facts with system-attested provenance while
keeping boot output readable and the runtime cost low.

The system log MUST:

- display readable lines during boot, including before persistent storage is
  writable;
- store structured records that are searchable without parsing prose;
- rotate and expire old data by segment, without rewriting live records;
- make audit tampering, insertion, deletion, truncation, and source spoofing
  detectable;
- keep the hot path cheap in CPU, allocation, lock contention, and storage;
- let a caller write a useful record with no registry, no template file, and no
  predeclared source identifier.

The system log MUST NOT:

- store diagnostic remediation text or operator advice as part of the log format;
- require event-definition files, rendering templates, global source IDs, or a
  central event registry;
- trust caller-supplied source, origin, stream, timestamp, sequence, integrity,
  or audit metadata;
- require text parsing to recover structured fields;
- treat Markdown, JSON, or terminal text as the authoritative on-disk format;
- allow a user process to make its records look as if they came from the kernel,
  a different service, audit, security, or a privileged driver.

## 2. Authority model

A log record has two different classes of data:

1. **System-attested metadata**, produced by the kernel, trusted runtime, or
   journal service.
2. **Caller content**, supplied by the emitting component and treated as
   untrusted text or untrusted structured data unless the caller is a trusted
   TAIRiX component writing through an explicitly privileged path.

The caller may describe what it believes happened. The system records who said
it, where it came from, when it was accepted, which authority it carried, and
how it was appended.

### 2.1 System-attested fields

These fields are assigned by the logging system and MUST NOT be accepted from
ordinary userland as authority:

- `record.seq`
- `record.cpu_id`
- `record.cpu_seq`
- `record.boot_id`
- `record.stream`
- `record.effective_level`
- `record.monotonic_time`
- `record.wall_time`
- `record.wall_time_state`
- `origin.*`
- `source.name`
- `integrity.*`

If a caller attempts to provide one of these fields, the journal MUST either
reject the record or preserve the supplied value only as a caller claim. It MUST
NOT merge the caller value into the authoritative namespace.

### 2.2 Caller fields

Caller-controlled data is stored under caller-owned fields:

- `caller.message`
- `caller.component`
- `caller.tag`
- `caller.event_id`
- `caller.level`
- `caller.requested_stream`
- `caller.requested_source`
- `data.<key>`

`caller.message` is not authority. A malicious process can write a message such
as `login accepted for root`; the renderer and query tools MUST present that
message under the system-attested source and origin that actually produced it.

### 2.3 Streams

The journal assigns the effective stream. A caller may request a stream, but the
request is only a claim until the journal validates the caller's authority.

Canonical streams are closed:

| Stream | Purpose | Caller access |
|---|---|---|
| `boot` | Firmware handoff, kernel bring-up, early driver/storage/root path, and boot service startup | Kernel and trusted bootstrap components only |
| `runtime` | Ordinary process, service, driver, and application logs | Default for non-privileged emitters |
| `debug` | High-volume diagnostic logs with short retention | Scoped runtime access, rate-limited |
| `security` | Security-relevant allow/deny decisions and policy checks | Trusted kernel or security services only |
| `audit` | Audit-relevant state changes and privileged decisions | Trusted audit emitters only |
| `journal` | Journal self-events, loss records, seal records, rotation records, and verification failures | Journal service only |

Requests for `audit` or `security` from untrusted userland MUST NOT create audit
or security records. The request MAY be stored as `caller.requested_stream` on a
runtime record, and an attempted stream-spoof MAY itself produce a trusted
security record.

## 3. Source and origin

### 3.1 Origin

`origin` is the authority-bearing identity attached by the system. It is not
provided by the caller.

A complete origin SHOULD include:

- `origin.trust_domain`: one of `kernel`, `driver`, `system_service`,
  `user_service`, `app`, `user_process`, or `unknown`;
- `origin.uid`, `origin.gid`, and supplementary group summary where applicable;
- `origin.capability_summary`, excluding raw capability tokens;
- `origin.pid`;
- `origin.proc_id`, a kernel-generated process-instance identifier;
- `origin.ppid` and parent process instance when available;
- `origin.session_id` and login uid when available;
- `origin.service_id` for supervised services;
- `origin.package_id` or application bundle id when available;
- `origin.sandbox_id` or container id when available;
- `origin.executable_path`;
- `origin.executable_file_id`, identifying the executed file object;
- `origin.executable_digest` when the digest is already known or cheap to
  obtain;
- `origin.start_monotonic_time`.

`origin.proc_id` MUST distinguish two different lifetimes that reused the same
numeric PID. A recommended form is a 128-bit kernel-generated identifier
assigned at exec.

The log format MUST NOT rely on process names alone. Process names, argv text,
symlinks, copied binaries, and caller-controlled display labels are not strong
identity.

### 3.2 Source

`source.name` is a system-derived grouping label. It is authoritative for
filtering and display, but it is derived from origin and policy rather than
accepted from the caller.

Source derivation order:

1. Kernel context: `kernel.<subsystem>`.
2. In-kernel bootstrap floor driver: `driver.<class>.<driver_id>`.
3. Supervised system service: `service.<service_id>`.
4. Supervised user service: `user.<uid>.service.<service_id>`.
5. Signed application bundle: `app.<bundle_id>`.
6. Unsigned or unsupervised user process: `user.<uid>.proc.<proc_id>`.
7. Fallback when identity is incomplete: `unknown.<proc_id>` or
   `unknown.kernel` for early records before full identity exists.

A caller MAY provide `caller.component` or `caller.tag` for local grouping.
Those values MUST NOT replace or prefix `source.name`.

Renderer examples:

```text
[  1.420] error kernel.net: dhcp timeout iface=net0 elapsed=10s
[ 12.440] warn  service.backup[scanner]: file skipped path=/Storage/a errno=EACCES
[ 44.120] warn  user.1000.proc.p01jd8... requested_source=kernel.audit: audit disabled
```

The final line is a spoofing attempt preserved as evidence. It is not displayed
as a kernel audit record.

### 3.3 Reserved names

The following source prefixes are reserved and MUST NOT be granted to ordinary
userland:

- `kernel.*`
- `driver.*`
- `audit.*`
- `security.*`
- `journal.*`
- `service.*`
- `system.*`

A trusted service obtains its `service.<service_id>` source through its signed
manifest and supervisor identity, not by passing the string to the logger.

## 4. Caller API model

The common logging API is message plus fields:

```rust
log::warn("dhcp timeout", fields!("iface" => iface, "elapsed" => elapsed));
```

The caller does not register a source, does not allocate a source ID, does not
predeclare a record shape, and does not maintain a rendering template.

Minimum valid caller record:

```text
caller.message = "started"
```

Normal structured caller record:

```text
caller.message = "dhcp timeout"
data.iface = "net0"
data.elapsed = 10s
```

Security or audit-relevant trusted record:

```text
caller.event_id = "auth.login.denied"
caller.message = "login denied"
data.user = "ian"
data.reason = "bad_password"
```

The public API SHOULD make the easy path structured by default. A caller should
not need to format values into the message string to produce readable output.

### 4.1 Message rules

`caller.message` is a short, stable, human-readable summary. It SHOULD NOT carry
high-cardinality values that belong in fields.

Preferred:

```text
message = "dhcp timeout"
iface = "net0"
elapsed = 10s
```

Discouraged:

```text
message = "dhcp timeout on net0 after 10s"
```

The discouraged form is still accepted for usability. Tooling MAY report it as
a lint when it causes poor aggregation or search.

### 4.2 Event IDs

`caller.event_id` is optional for runtime and debug records. It is REQUIRED for:

- security allow/deny decisions;
- audit-relevant state changes;
- journal integrity events;
- driver match, load, skip, and failure decisions;
- any record written to the `security` or `audit` streams.

An event ID is a stable, source-local ASCII identifier such as
`auth.login.denied` or `driver.load.denied`. It does not require a global
registry, an event-definition file, or a renderer template. Stability is a
contract of the emitting trusted component.

For untrusted runtime records, `caller.event_id` is caller content. It is useful
for filtering, but it is not proof that the named event actually occurred.

### 4.3 Levels

The caller may provide `caller.level`. The journal assigns
`record.effective_level` after stream and origin policy are applied.

Canonical caller levels are closed:

- `trace`
- `debug`
- `info`
- `warn`
- `error`
- `critical`

Untrusted callers may label their own events `critical`, but the renderer MUST
make the source and stream clear. A user runtime record MUST NOT be displayed as
a kernel, audit, or system-critical event merely because its caller level was
`critical`.

### 4.4 Field names

Caller field names are case-sensitive ASCII identifiers:

```text
[a-z][a-z0-9_]{0,63}
```

The journal stores them as `data.<name>`. Caller fields MUST NOT use reserved
prefixes such as `record.`, `origin.`, `source.`, `integrity.`, or `sys.`.

Common field names are conventions, not a schema:

- `actor`
- `subject`
- `object`
- `action`
- `result`
- `reason`
- `errno`
- `path`
- `iface`
- `device`
- `driver`
- `service`
- `pid`
- `uid`
- `gid`
- `duration`
- `count`
- `bytes`
- `op`
- `cause`

A component may use other names without registration.

### 4.5 Field types

Value types are closed:

- `null`
- `bool`
- signed integer
- unsigned integer
- fixed-point decimal
- `Time64`
- `Duration64`
- UTF-8 string
- bounded bytes
- UUID
- IP address
- MAC address
- error code
- capability id, never a raw token
- same-type bounded list of any scalar type above

Nested maps are forbidden in log records. A record is a flat set of typed
fields so search, indexing, validation, and rendering stay cheap.

Fields that might contain secrets MUST NOT be logged. Secret wrapper types MUST
not implement the log-value conversion trait. Logging a secret by converting it
to a string is a defect in the caller.

## 5. Record model

A logical record contains:

```text
record:
  format_version
  seq
  cpu_id
  cpu_seq
  boot_id
  stream
  effective_level
  monotonic_time
  wall_time
  wall_time_state

origin:
  trust_domain
  uid
  gid
  pid
  proc_id
  service_id
  package_id
  sandbox_id
  executable_path
  executable_file_id
  executable_digest
  start_monotonic_time
  capability_summary

source:
  name

caller:
  level
  component
  tag
  event_id
  requested_source
  requested_stream
  message

fields:
  data.*

integrity:
  previous_record_hash
  record_hash
  segment_id
  segment_hash
```

Not every origin field is available for every record. Missing origin data MUST
be represented explicitly as absent, not guessed.

### 5.1 Time

All absolute time values use `Time64`. Relative and monotonic values use
`Duration64`-equivalent signed seconds plus nanoseconds.

`record.monotonic_time` is the ordering time within a boot. It is mandatory.

`record.wall_time` is optional until the wall clock has been established. The
journal MUST preserve `record.wall_time_state` so readers can distinguish:

- `unset`: wall time was unavailable;
- `firmware`: supplied by firmware or RTC before validation;
- `trusted`: set by a trusted time source;
- `adjusted`: record occurred after a wall-clock correction.

Search and verification order MUST use sequence and monotonic time, not wall
time.

### 5.2 Sequences

The journal assigns `record.seq`, a monotonic append sequence for the stream.
The emitting CPU or kernel ingress path supplies `record.cpu_id` and
`record.cpu_seq` where applicable.

`record.cpu_seq` is monotonic per CPU and is used to detect lost early-buffer
records and per-CPU ingestion gaps. It is not a replacement for the append
sequence.

### 5.3 Causality

Any caller may include:

- `data.op`: operation identifier;
- `data.cause`: prior `record.seq` or operation identifier;
- `data.parent`: parent operation identifier.

The journal stores these fields without interpreting them. Diagnostic tools MAY
use them for grouping, but the log format itself does not carry recommendations
or remediation instructions.

## 6. Physical storage

The authoritative on-disk format is compact binary segments under
`/System/Logs`. Text, JSON, Markdown, and HTML are renderings or exports only.

Recommended layout:

```text
/System/Logs/
├── boot/
├── runtime/
├── debug/
├── security/
├── audit/
├── journal/
├── index/
└── Anchors/
```

The exact directory names under each stream are implementation details, but
stream separation is mandatory so retention and authority are enforceable
without rewriting mixed-class files.

### 6.1 Segment files

A segment is append-only. It contains:

```text
SegmentHeader
Record...
SegmentFooter
```

The segment-local string dictionary (§6.2) is **not** a separate block. It is
carried inside the records by back-reference: the first record to use a
repeated string carries it inline and defines a segment-local handle, and later
records reference the handle. The dictionary is therefore reconstructed by
reading the records in order and needs no block, offset table, or digest of its
own — it is already covered by the record hash chain and the segment hash that
protect those record bytes.

The header contains:

- segment magic and format version;
- stream;
- segment id;
- machine id hash;
- boot id;
- first append sequence;
- previous segment hash;
- creation monotonic time;
- creation wall time if available.

The footer contains:

- record count;
- first and last append sequence;
- first and last monotonic time;
- last record hash;
- segment hash;
- previous segment hash;
- optional seal signature or MAC;
- footer checksum.

An open segment may lack a footer. Recovery scans forward to the last complete,
checksum-valid committed record.

### 6.2 Segment-local dictionaries

The physical format encodes repeated strings with a segment-local dictionary,
so a stream of records from `kernel.mem` stores that source name once and
references it thereafter — compact without any registration.

The dictionary is a **back-reference** codec, not a stored table. Each
dictionary-eligible string is coded as one of three forms: inline-and-forgotten
(`plain`), inline-and-defines-the-next-handle (`def`), or a reference to an
earlier definition (`ref`). Because a handle only ever names a string an
earlier record already carried inline, the writer and reader assign handles in
lockstep by walking the segment's strings in the same field-and-record order;
no handle number is stored on the definition, and there is no dictionary block
or dictionary digest to keep in sync.

The strings compressed this way are the low-cardinality provenance and summary
strings — the system-derived source name and the caller's component, tag, event
id, requested source, and message. High-cardinality caller `data.*` names and
values are left inline (§6.3): they rarely repeat and would only churn the
handle space.

Dictionary handles are private to one segment. They are not public source IDs,
field IDs, or event IDs. A handle in one segment has no meaning in another.

Search tools reconstruct a segment's dictionary by reading its records in order
and MAY use segment summaries, hashes, and bloom filters to skip segments that
cannot match.

### 6.3 High-cardinality strings

The writer MUST bound dictionary growth. Repeated short strings SHOULD be
dictionary-compressed. High-cardinality strings SHOULD remain inline unless
they repeat enough to justify promotion.

Examples of strings that SHOULD usually remain inline:

- request IDs;
- random tokens;
- long paths;
- URLs;
- user input;
- stack traces;
- cryptographic digests;
- one-off error text.

The journal MUST rate-limit or reject attempts to exhaust dictionaries with
unique source, message, tag, or field-name values.

### 6.4 Record size limits

The implementation MUST define hard limits for:

- message length;
- component and tag length;
- event ID length;
- field count;
- field name length;
- string and bytes value length;
- total encoded record size;
- per-segment dictionary size.

Oversized untrusted records MUST be rejected, not partially applied. The journal
MAY emit a trusted loss record summarizing rejected record counts by origin.

Audit and security records SHOULD be small enough that rejection indicates a
programming defect in the trusted emitter.

## 7. Integrity and tamper resistance

### 7.1 Record hash chain

Every committed record contributes to a hash chain:

```text
record_hash = H(canonical_record_bytes_without_record_hash || previous_record_hash)
```

The first record in a segment chains to the previous segment hash. The first
segment in a stream chains to a stream genesis value bound to the machine id,
stream, and boot id.

The hash algorithm is supplied by `lib/crypto`. The logging implementation MUST
NOT hand-roll cryptographic primitives.

### 7.2 Segment sealing

When a segment closes, the journal computes the segment hash over the header,
dictionary state, committed records, and footer fields excluding the footer's
own seal.

For audit and security streams, closed segments MUST be sealed with a key that
ordinary services cannot read. Recommended sealing modes are:

- keyed MAC with a log-attestation key protected by the platform key store;
- signature with the per-installation log-attestation key;
- forward-secure key evolution where old sealing keys are erased after use.

Runtime, debug, and boot streams MUST at least be hash-chained and checksummed.
They MAY also be sealed.

### 7.3 Anchors

The current log root hash MUST be signed and persisted under
`/System/Logs/Anchors/` at least once per minute and on clean shutdown.

An anchor record contains:

- machine id hash;
- stream;
- boot id;
- segment id;
- last append sequence;
- last per-CPU sequence summary;
- root hash;
- wall time and wall time state;
- signer id;
- signature.

A discontinuity in any hash chain, missing anchor, invalid signature, sequence
gap, or unexpected truncation is itself a security event.

### 7.4 Rotation and truncation

No capability may edit an existing committed record. Rotation and expiry delete
or archive whole sealed segments.

Truncation or segment removal requires `CAP_LOG_ROTATE`. The action MUST create
a trusted journal record containing:

- actor origin;
- stream;
- affected segment range;
- first and last sequence removed;
- hashes of removed segments;
- reason;
- retention policy id.

Expired audit and security segments MUST NOT be removed until they are sealed
and anchored. Removing old segments MUST leave enough anchor metadata to prove
what boundary was intentionally removed.

## 8. Boot logging and rendering

### 8.1 Early ring buffers

Before persistent storage is available, the kernel writes records to bounded
per-CPU memory ring buffers. These buffers use the same logical record model as
persistent logs, minus unavailable origin details.

Once `/System/Logs` is writable, the journal imports early records into the
`boot` stream, preserving monotonic time, CPU id, CPU sequence, boot id, and
hash-chain continuity from the import boundary onward.

If early records are overwritten before import, the journal MUST emit a trusted
loss record with the affected CPU and sequence range.

### 8.2 Boot console renderer

The boot renderer subscribes to trusted boot records and prints generic readable
lines. It does not need event templates.

Canonical line shape:

```text
[monotonic] level source[component]: message key=value key=value
```

Example:

```text
[  0.064] info  kernel.mem: detected usable=32768MiB
[  0.903] info  kernel.fs: mounted path=/ readonly=true type=arxfs
[  1.420] error kernel.net: dhcp timeout iface=net0 elapsed=10s
```

The renderer MUST escape control characters in caller-controlled text. Caller
messages, components, tags, and fields MUST NOT be allowed to move the cursor,
change colour, clear the screen, forge prefixes, or create fake lines.

The renderer MUST NOT add remediation text. Its job is to display recorded
facts, not to advise the operator.

### 8.3 Markdown and rich renderers

Markdown, terminal-table, JSON, and HTML renderers are views over structured
records.

A Markdown boot report MAY group records by boot phase, stream, source, level,
operation id, or cause relation. It MUST preserve provenance and distinguish
system-attested metadata from caller content.

Markdown output is never the canonical log. Editing a Markdown report does not
change the log.

## 9. Search and indexes

Search is structured first and text second.

Required query dimensions:

- stream;
- boot id;
- sequence range;
- monotonic time range;
- wall time range with wall time state;
- effective level;
- caller level;
- source name;
- origin uid, gid, pid, proc id, service id, package id, sandbox id;
- message exact match;
- message substring match;
- event id;
- component;
- tag;
- field presence;
- typed field comparison;
- operation id;
- cause reference;
- integrity status.

Example queries:

```text
log find stream=audit event_id=auth.login.denied
log find source=service.devmgr level>=warn
log find origin.uid=1000 message="file skipped"
log find data.iface=net0 data.elapsed>5s
log find integrity.status!=valid
```

Indexes are disposable accelerators. The segment files and anchors are the
authority. Removing `/System/Logs/index` and rebuilding it MUST NOT change log
meaning.

The indexer MUST validate segment checksums and hash chains while indexing. It
MUST mark records from unverifiable segments as integrity-failed rather than
silently indexing them as normal.

## 10. Retention

Retention is stream-specific and segment-based.

A retention policy may constrain:

- maximum total bytes per stream;
- minimum free space;
- maximum age;
- minimum number of boots to keep;
- debug stream budget;
- audit/security seal and anchor requirements;
- whether export/archive is required before deletion.

Records are not individually deleted from the middle of a segment. If a segment
contains records with different retention requirements, the stricter
requirement wins. Implementations SHOULD avoid mixed-retention segments by
keeping streams separate.

Debug and trace records MAY have short retention and aggressive aggregation.
Audit and security retention MUST be conservative and MUST NOT silently discard
records because of storage pressure.

## 11. Loss, rate limiting, and aggregation

The logging system must protect the machine from log-driven denial of service.

Runtime and debug streams MAY be rate-limited, sampled, aggregated, or dropped
under pressure. Any such loss MUST produce a trusted journal loss record when
the journal can still write.

Audit and security streams MUST NOT silently drop records. If an audit/security
record cannot be accepted, the caller receives an error and the system follows
the configured fail-closed policy for that decision path.

Repeated equivalent records MAY be coalesced into an aggregate record:

```text
caller.event_id = "journal.aggregate"
caller.message = "records coalesced"
data.original_message = "packet dropped"
data.count = 9842
data.window = 5s
data.source = "service.net"
```

Aggregation MUST preserve source, origin class, level, stream, and the field
values that define equivalence. Audit and security records MUST NOT be
coalesced unless the trusted event definition explicitly states that the count
is the audit fact being recorded.

## 12. Security boundaries

### 12.1 Write path

Ordinary processes do not append to segment files directly. They write through a
logging syscall, IPC endpoint, inherited log handle, or trusted runtime path.
The kernel or journal ingress path attaches origin before the record reaches
persistent storage.

`CAP_LOG_WRITE` is partitioned by service and stream. Possessing authority to
write runtime records for one service does not allow forging another service's
source or writing audit/security records.

The journal service is the only component that writes authoritative segment
files in steady state. Kernel early boot is the exception before the journal
service exists.

### 12.2 Spoofing attempts

The journal MUST defend against:

- requested privileged source names;
- requested audit/security streams;
- reserved field prefixes;
- terminal escape injection;
- newline injection;
- excessive source/component/tag cardinality;
- high-volume record floods;
- backdated wall times;
- caller-supplied sequence numbers;
- caller-supplied origin or capability data;
- raw capability tokens in fields;
- secrets in known secret types.

A spoofing attempt SHOULD be preserved as caller content when safe to do so. The
trusted source and origin remain the system-derived values.

### 12.3 Secret handling

Logs MUST NOT contain secrets, raw credentials, private keys, raw capability
tokens, session tokens, password material, or plaintext data whose sensitivity
is known to the caller.

The logging API MUST make known secret-bearing types unloggable by default. A
caller that wants to record the existence of a secret-bearing object records a
non-sensitive handle, class, or digest only when that digest is itself safe to
expose.

The journal MUST NOT rely on lossy redaction as the primary defence. Rejecting
or preventing secret logging is preferred to storing a transformed secret.

## 13. Validation and recovery

`log verify` MUST check:

- segment header and footer checksums;
- record checksums;
- record hash chain;
- segment hash chain;
- seal signatures or MACs;
- anchor signatures;
- append sequence monotonicity;
- per-CPU sequence monotonicity;
- stream/source authority consistency;
- unexpected truncation;
- dictionary resolution (every reference resolves to an in-segment definition;
  covered by the record chain and segment hash, so it needs no separate
  dictionary digest).

Verification results are records in the `journal` stream when the journal is
writable. A verification failure is a security event.

Power-loss recovery scans open segments to the last committed valid record,
closes or quarantines the damaged tail, emits a journal recovery record, and
continues with a new segment. Recovery MUST NOT rewrite valid committed
records.

## 14. CLI tools

The required user-facing tools are:

```text
log boot                 # show current boot summary
log show                 # show records
log tail                 # follow records
log find                 # structured search
log verify               # verify hashes, seals, anchors, and indexes
log expire --dry-run     # show retention action without applying it
log report --format md   # render a Markdown report from records
log export --format json # export structured records
```

Tool output uses standard streams. Primary data goes to stdout, diagnostics to
stderr, and optional command metadata to stdinfo. Security and audit events go
through `lib/log`, not stdinfo.

The tools MUST NOT read `/proc`, `/sys`, or device-specific paths. Runtime
system data comes from TAIRiX APIs and the log files under `/System/Logs`.

## 15. Implementation placement

The shared logging data model, encoders, decoders, query primitives, and
renderers live in `lib/log` unless they need a narrower home.

The journal service is a userland system service. Kernel code owns early boot
ring buffers, trusted origin attachment, and privileged audit/security ingress.

Architecture-specific code is limited to the minimum pieces required to obtain
per-CPU identity, monotonic time, early boot output, and platform sealing
support. Shared record encoding, hashing layout, source derivation rules,
retention logic, and renderers are architecture-neutral.

No C or C++ source is part of this design.

## 16. Tests and review requirements

The implementation MUST include tests for:

- encoding and decoding every value type;
- rejection of malformed records;
- reserved namespace spoofing;
- source spoofing;
- stream spoofing;
- terminal escape rendering;
- high-cardinality dictionary abuse;
- record size limits;
- rate-limit loss records;
- audit/security non-dropping behaviour;
- segment hash-chain verification;
- segment truncation detection;
- invalid anchor detection;
- power-loss recovery;
- sequence gaps;
- pre-1970 wall time;
- post-2038 wall time;
- absent, firmware, trusted, and adjusted wall-time states;
- index rebuild from segments;
- Markdown rendering preserving provenance;
- headless boot rendering.

Every public decoder and untrusted ingest path requires fuzz coverage. Every
security decision emitted through the log path requires a regression test that
verifies the stable event ID, stream, source derivation, and absence of forged
caller authority.

## 17. Non-goals

The system log is not:

- a syslog compatibility protocol;
- an observability vendor format;
- a Markdown log file;
- a JSONL canonical store;
- a template-rendered event catalogue;
- a runbook engine;
- an operator-advice system;
- a way to bypass standard streams;
- a substitute for the System Information API.

Compatibility exporters may be added later as separate tools, but exporters do
not define the authoritative TAIRiX log format.
