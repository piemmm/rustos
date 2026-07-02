# `rustos-log`

Structured, level-filtered, allocation-free logging.

## Model

* `Event` — a borrowed record carrying a `Level`, a stable `EventId`, a
  short message, and an optional slice of `Field`s. A `Field` is a `&str`
  key plus a typed `FieldValue` — the one field-value model (below), so a
  caller logs a real integer, error code, capability id, address, or bounded
  string rather than a pre-formatted string. Console sinks render a value
  through its `Display` impl.
* `Sink` — trait implemented by anything that can receive an `Event`
  (a serial port, a kernel ring buffer, an IPC pipe). Sinks must not
  panic and must not allocate on the hot path.
* `log(sink, event)` — single dispatch entry point; performs one
  relaxed-atomic load and one comparison before deciding whether to
  forward the record.

A single global `AtomicU8` holds the current `max_level`; values below
the threshold are dropped before reaching the sink, so a `Trace` call in
production code costs the same as an inlined branch.

## Event identifiers

`EventId` is a `u32` newtype. The numeric values are part of the contract
with external log consumers (operator dashboards, audit pipelines): once
published, an ID is fixed forever. New events take the next free integer
inside their subsystem's reserved 1 000-wide range — for example
`1_000..2_000` for `kernel/sec`, `2_000..3_000` for `kernel/mem`.

## Tamper-evident audit chain (§19.4)

The `chain` module provides the cryptographic backbone for the
append-only security log under `/System/Logs` (`AGENTS.md` §19.4). See
[Audit-log integrity](../security/audit_log.md) for the full model.

* `LogChain` — one per stream; `append(cpu, payload_digest)` issues a
  `ChainedEntry` binding the previous entry's hash, a monotonic append
  sequence number, the record's originating CPU id (bound as evidence),
  and the caller-supplied payload digest. The append path hashes a single
  fixed-size stack buffer and never allocates.
* `ChainedEntry` — a self-describing record; `recompute_hash` /
  `is_self_consistent` re-derive its hash so a verifier never trusts a
  stored hash it did not recompute.
* `verify_chain` / `verify_fresh_chain` — walk a slice of entries,
  reporting the first `ChainError` (`HashMismatch`, `BrokenLink`,
  `SequenceGap`) and otherwise returning the chain root. A tampered `cpu`
  is caught as `HashMismatch`, since it is bound into each entry's hash.

The module is payload-format agnostic: the persisted-log writer reduces
each serialized record to a `rustos-crypto` SHA-256 digest, so this
crate pulls in no payload codec and stays allocation-free.

## Streams and on-disk segments

* `Stream` — the closed set of log streams (`boot`, `runtime`, `debug`,
  `security`, `audit`, `journal`). The discriminant is a stable on-disk
  value; `genesis_label` feeds `stream_genesis`, and audit/security
  streams `requires_seal`.
* `segment` — the append-only on-disk container for one stream: a
  self-checksummed `SegmentHeader`, length-framed records each carrying
  their `LogChain` link hash and their own monotonic ordering time
  (`plans/SYSLOG.md` §5.1; covered by the segment hash, not the per-record
  chain, exactly like the append sequence), and a `SegmentFooter` with the
  record / sequence / time bounds, the segment hash, an optional seal MAC
  (mandatory for audit/security), and a footer checksum. `SegmentWriter`
  builds one into a caller buffer; `SegmentReader` is a forward-scanning,
  self-verifying reader that recovers to the last complete chain-valid
  record after a torn write; `verify_segment` fully checks a closed
  segment and returns a `SegmentSummary`. The container is
  payload-agnostic (record bytes are opaque) and every hash is over a
  contiguous byte range, so no streaming hash is needed.

## Logical record model (`record`)

A committed log entry has two layers. The **physical container** (`segment`,
above) owns the record's stream, append sequence, originating CPU, monotonic
time, boot id, and the whole integrity group (per-record and per-segment
hashes, the optional seal). The `record` module owns the **logical record
body** the container carries as an opaque payload — everything else in the
`plans/SYSLOG.md` §5 model, encoded once and never duplicating a
container-owned field.

* `LogRecord` — the borrowed encode input: the authoritative
  `effective_level` (`Level`), the per-CPU `cpu_seq`, the per-record
  `WallClockReading` (wall time + trust state), the kernel-attested `Origin`,
  the system-derived `source_name`, the `CallerContent`, and the flat set of
  `data.*` `(FieldName, FieldValue)` pairs. `encode` writes a compact
  little-endian body against the segment's `DictionaryBuilder` (below),
  checking every bound and rejecting a violating record whole.
* `CallerContent` — the caller-supplied portion the journal stores faithfully
  but never treats as authority: the optional caller `level`, `component`,
  `tag`, `event_id`, `requested_source`, and `requested_stream`, plus the
  required `message`.
* `decode` / `LogRecordRef` — a fail-closed decoder returning a validated
  borrowed view, resolving the record's dictionary-coded strings through the
  segment's `DictionaryView` (below). Every length, discriminant, and UTF-8
  constraint is checked, each `data.*` key is re-validated against the
  `FieldName` grammar, and the fields must tile the body exactly (no trailing
  bytes). `DataFieldIter` walks the validated `data.*` pairs.

## Segment-local string dictionary (`dict`)

Provenance and message strings repeat heavily within a stream, so `record`
encodes its low-cardinality strings — the system-derived `source_name` and the
caller's `component`, `tag`, `event_id`, `requested_source`, and `message` —
through a per-segment dictionary that stores a repeated string once and
references it thereafter (`plans/SYSLOG.md` §6.2/§6.3). High-cardinality
`data.*` names and values stay inline by policy.

The dictionary is a **back-reference** codec, not a stored table: each string
is coded as inline-and-forgotten, inline-and-defines-the-next-handle, or a
reference to an earlier definition. The writer and reader assign handles in
lockstep by walking a segment's strings in the same field-and-record order, so
no handle number is stored on a definition and there is no dictionary block or
digest to keep in sync — the strings are already covered by the record hash
chain and segment hash.

* `DictionaryBuilder` — the writer-side dictionary for one segment. It decides
  inline vs. promote vs. reference and enforces bounded growth: a string is
  promoted to a handle only on its **second** sighting (a unique string is
  never remembered), the entry and candidate tables are fixed-capacity
  (`MAX_ENTRIES` / `MAX_CANDIDATES`) over a fixed byte arena (`ARENA_BYTES`),
  and a string longer than `MAX_DICT_STRING` is never interned. Once full,
  further strings simply stay inline, so a flood of unique short strings cannot
  exhaust it.
* `DictionaryView` — the reader-side dictionary. It accumulates the strings
  definitions carry (borrowed from the segment bytes) and resolves references
  against them, fail-closed: a definition past `MAX_ENTRIES` or a reference to
  an undefined handle is rejected.

The records of one segment are encoded through one builder, and decoded through
one view, in append order. The advancing byte cursors that `record` and `dict`
share live once in an internal `cursor` module (one definition), distinct from
`rustos_abi`'s offset-indexed `le` helpers for fixed-`WIRE_LEN` structs.

The body reuses the shared building blocks — `FieldValue`/`FieldName`,
`Origin`, `WallClockReading`, `Stream`, `Level`, and the shared named-field
codec (`rustos_abi::encode_named_field` / `decode_named_field`) that the
`log_emit` diagnostic record also builds on — so there is one field-encoding
definition, not two. Origin fields the kernel cannot yet attest are absent by
construction rather than guessed.

## Early-boot ring buffers (`bootring`)

Before `/System/Logs` is writable the kernel still produces the earliest and
most diagnostic records — memory sizing, hardware discovery, driver bring-up
(`plans/SYSLOG.md` §8.1). Each CPU owns one `BootRing`: a bounded,
allocation-free FIFO over a caller-owned byte arena that retains its most
recent records until the journal can import them into the `boot` stream.

* A ring stores the *same* logical record body the persistent path uses
  (`record`, above) as an opaque blob, plus the two container-owned facts the
  body does not carry that import must preserve: the per-CPU record sequence
  (`cpu_seq`) and the monotonic time the record was produced. The producer
  supplies `cpu_seq` — it owns the per-CPU counter the encoded body's own
  `cpu_seq` already reflects — so the ring never invents a sequence that could
  disagree with the body; it accepts values only strictly increasing and
  rejects anything else fail-closed.
* `push` writes one frame at the tail, wrapping the physical end of the arena
  (frames are never split-padded, so no space is wasted). When the ring is
  full it evicts the oldest frame(s) to make room: a boot ring keeps *recent*
  history, never blocks the boot path, and never grows without bound. A body
  larger than the whole ring, or larger than `MAX_BOOT_RECORD_BODY`, is
  rejected rather than truncated.
* Eviction is never silent. The ring accumulates the contiguous `cpu_seq`
  range of every record dropped before it was drained; `take_loss` returns
  that `LossRange` (CPU id, first/last sequence, count) so the journal emits
  one trusted loss record naming the affected CPU and range rather than
  leaving an undetectable gap.
* `pop_oldest` drains the oldest record in FIFO order, copying its body into a
  caller-supplied scratch buffer and returning the preserved `cpu_seq` and
  monotonic time. The journal calls `take_loss` first, then drains, so the
  loss record precedes the surviving records.

Like `SegmentWriter`, a `BootRing` is not internally synchronised: it has one
writer (its own CPU) and is drained once, at import, after that CPU stops
writing. It reuses `Duration64` and the crate's fail-closed idioms; no
allocation occurs on the boot path.

## Authority model (`authority`)

A record has two classes of data: **system-attested** metadata the kernel or
journal vouch for, and **caller content** the emitter chose (`plans/SYSLOG.md`
§2). The `authority` module owns the two decisions that turn an attested
`Origin` plus the caller's *requests* into the authoritative values a record
carries. Both are `no_std` and allocation-free, so they run on the kernel's
early-boot path.

* `derive_source` — computes the system-derived `SourceName` from the attested
  `Origin`, never from anything the caller supplied (§3.2). A `Kernel`-domain
  origin with a valid subsystem label becomes `kernel.<subsystem>`; a
  `User`-domain origin becomes `user.<uid>.proc.<proc_id_hex>`; a kernel record
  with no (or a grammar-violating) subsystem falls back to `unknown.kernel`.
  The subsystem label is trusted kernel input but is still grammar-checked, so
  it can never smuggle a `.` and synthesise `kernel.audit.…`. `SourceName` is a
  fixed-capacity inline buffer and there is no constructor that accepts a
  caller string. The source-derivation order in the specification also names
  driver / supervised-service / signed-app classes; those need executable-role
  metadata the kernel does not yet attest (`Origin` distinguishes only
  `Kernel` from `User` today), so they are added in place here when their
  attestation producer exists rather than invented ahead of it.
* `reserved_source_prefix` / `RESERVED_SOURCE_PREFIXES` — screen a caller's
  advisory `caller.requested_source` for a reserved prefix (`kernel.`,
  `driver.`, `audit.`, `security.`, `journal.`, `service.`, `system.`). A hit
  is a spoofing attempt: it is preserved as a caller claim (evidence), never
  allowed to become the authoritative source (§3.3).
* `resolve_stream` — assigns the effective `Stream` from the caller's requested
  stream and the origin's trust (§2.3), returning a `StreamDecision`. A
  `Kernel`-domain principal is trusted for every stream, so its request is
  honoured. A `User`-domain principal may write only the caller-writable
  streams (`runtime`, `debug`); a request for a trusted-emitter stream
  (`boot` / `security` / `audit` / `journal`, per `Stream::requires_trusted_emitter`)
  is denied, downgraded to `runtime`, and `spoofed` is set so the ingress can
  preserve the request as a claim and raise a trusted security record. An
  absent request defaults to `runtime`. Finer trust (a supervised service that
  may legitimately write `security`/`audit`) grows in place when the kernel
  attests that trust domain.

## Record ingress (`ingress`)

Ingress is the point where an untrusted caller's request becomes an
authoritative record (`plans/SYSLOG.md` §2.1, §5.2). The kernel ingress path
supplies the facts it alone can attest — the emitter's `Origin`, its per-CPU
`cpu_seq`, and the monotonic/wall readings — and the caller supplies its
content and its *requests*. `Ingress::admit` combines them, applying the
`authority` decisions above and assigning the one authoritative fact the
caller can neither pick nor skip: the per-stream append sequence.

* `Ingress` — owns the next append `seq` for each of the `STREAM_COUNT`
  streams and is the single writer of those counters, so a record's `seq` is
  monotonic within its stream regardless of what a caller requests. `new`
  starts every counter at zero; `resume` seeds them from each stream's last
  committed `seq + 1` so a restart or a new segment continues the sequence
  rather than reusing one. `next_seq` reads a counter without consuming it
  (for anchoring and segment-header seeding).
* `admit` — resolves the effective stream (`resolve_stream`), derives the
  authoritative `SourceName` (`derive_source`), screens the caller's advisory
  `requested_source` for a reserved-namespace spoof (`reserved_source_prefix`),
  assigns the effective level (the caller's level, or `Info` when absent — the
  source and stream, not the level, carry authority, so even a user-labelled
  `critical` is honoured as the level), and consumes one append sequence for
  the resolved stream. It returns an `Admission`.
* `Admission` — the decision: the stream, `seq`, effective level, derived
  source, and attested origin, plus the `stream_spoofed` / `source_spoofed`
  flags. `build_record` assembles the `LogRecord` body once the container-owned
  `cpu_seq`/`wall` are known, carrying the caller's `requested_stream` /
  `requested_source` through verbatim as claims — so a spoof is preserved as
  evidence under the authoritative source, never as authority.

Ingress deliberately stops at the admission decision: it does not write
segments, detect per-CPU sequence gaps, rate-limit, apply retention, or emit
the trusted security record a spoof warrants. Persistence is the `journal`
engine below; the remaining service concerns build on top of the `Admission`
this returns.

## Journal engine (`journal`)

The `journal` engine turns admitted records into durable, hash-chained,
per-stream segments (`plans/SYSLOG.md` §6–§8). It sits above `Ingress` and
below the concrete storage: it owns the append-sequence authority and the
per-stream segment state, and drives the segment lifecycle over a
caller-supplied sink. It is storage-agnostic — it never names a filesystem
syscall — so the FS-backed store and the IPC ingress endpoint are the userland
journal service's concern, layered on top. It is `no_std` and allocation-free,
and every path fails closed.

* `SegmentStore` — the sink trait. The journal calls `store_segment` once per
  closed segment, in append order within a stream, passing the whole immutable
  segment image. The concrete store is a directory under
  `/System/Logs/<stream>/` reached over the filesystem syscalls.
* `Journal` — constructed with one working buffer per stream, the
  installation/boot binding, the log-attestation key (required to close
  `audit`/`security` segments), and the journal's own attested origin.
  * `admit` — a pass-through to the owned `Ingress`, so admit and commit are
    1:1 (each reserves exactly one append sequence).
  * `admit_limited` — the caller-facing admission path: it applies the
    per-stream rate limit **before** reserving a sequence and returns
    `Some(Admission)` within the rate or `None` when a `runtime`/`debug`
    record is over its rate (dropped best-effort, no sequence consumed, no
    spoof note — so a spoof flood is bounded at the runtime rate rather than
    amplified into a flood of `security` records). The system-authority
    streams are never dropped here.
  * `emit_rate_loss` — drains the rate limiter's matured drop tallies into one
    coalesced trusted `journal.rate.loss` record per stream per interval, so a
    flood is never a silent gap and never a second flood of loss records.
    `with_rate_limit` installs the policy (a journal defaults to no limiting).
  * `commit` — encodes an admitted record and appends it to its stream's open
    segment, **rotating** — closing, sealing, persisting, and reopening a fresh
    segment that chains onto the one just closed — when the segment fills. A
    segment is opened with `first_seq` set to the record's reserved sequence,
    so the segment's own record chain and the `Ingress` counter stay in
    lockstep. An invalid or over-cap record is rejected whole (`JournalError`),
    never partially written, and the segment's dictionary is discarded so it
    stays consistent with the records the segment actually holds.
  * `import_boot` — drains a CPU's early-boot `BootRing` into the `boot` stream,
    appending each retained body verbatim (a boot body is a self-contained,
    dictionary-free record body, so it needs no re-encoding). If the ring
    evicted records first, one trusted loss record naming the lost CPU-sequence
    range is authored on the `journal` stream, so a boot-log reader sees an
    explicit gap rather than a silent one.
  * `note_spoof` — authors a trusted record on the `security` stream when an
    `Admission` came back spoofed (a caller requested a privileged stream it
    was not trusted for, or a source impersonating a reserved namespace). The
    authoritative record was already committed under the caller's *derived*
    source and *downgraded* stream (preserving the request as a claim); this
    separate note, authored under the journal's own origin, records the attempt
    itself with the offending uid and the exact claims, so it is auditable
    independently of the record it concerned.
  * `flush` — closes every open segment (persisting each), keeping each
    stream's running chain hash so the next record reopens a chained segment.
    Called on shutdown and before anchoring.

The cross-segment chain uses each closed segment's `segment_hash` as the next
segment's `prev_segment_hash`; the append sequence is continuous across the
boundary. Loss and rotation self-events are authored on the `journal` stream,
and the spoof note on the `security` stream, through the same segment path —
never a second writer.

## Rate limiting (`ratelimit`)

The `ratelimit` module protects the machine from log-driven denial of service
(`plans/SYSLOG.md` §11). Only the two non-privileged, high-volume streams —
`runtime` and `debug` (`Stream::is_rate_limitable`, the one definition of which
streams are gated) — may be dropped; the four system-authority streams
(`boot`/`security`/`audit`/`journal`) are never dropped here and fail closed at
commit instead. It is `Copy`, `no_std`, and allocation-free, so a `Journal`
holds one directly with no allocator and no lock.

* `RateLimit::per_second(rate, burst)` — a per-stream token-bucket policy: one
  token is spent per admitted record, a token accrues every `1/rate` seconds,
  and the bucket holds up to `burst` tokens. The arithmetic is kept in integer
  nanoseconds (credit accrues at one nanosecond per nanosecond up to a
  `burst`-scaled cap), so there is no floating point and no accrual-rounding
  drift.
* `RateLimiter` — one bucket and one drop tally per gated stream.
  `RateLimiter::new` takes a policy per stream plus the reporting interval;
  `RateLimiter::unlimited` (the `Journal` default) never drops. `admit` accrues
  credit at the supplied monotonic time and returns `Admit` or `Drop`, folding
  each drop into the stream's tally (recording the window's first-drop time).
* `take_due_report` — drains a stream's tally into a `DropReport`
  (`stream`/`count`/`window`) once the reporting interval has elapsed since the
  first drop, coalescing a whole window of drops into one report and resetting
  the window; a sustained flood therefore yields at most one loss record per
  interval per stream. The `Journal` turns each report into one trusted
  `journal.rate.loss` record via `emit_rate_loss`.

## Boot-console rendering (`render`)

The `render` module turns a committed record into a readable boot-console line
(`plans/SYSLOG.md` §8.2). It is a pure, `no_std`, allocation-free formatter: the
kernel boot console renders each trusted record as it is produced, and later
tooling renders records read back from a segment; either way `render_line`
writes into any `core::fmt::Write` sink and needs no event templates or
registry.

* `render_line(out, monotonic, record)` — emits the canonical
  `[monotonic] level source[component]: message key=value key=value` line (no
  trailing newline). `monotonic` is the record's container-owned ordering time;
  `record` is a decoded `LogRecordRef`. The line is headed by the record's
  effective level and its **system-derived** source name; a caller's downgraded
  `requested_source` (a spoof the ingress path already rejected) is shown
  inertly as `requested_source=…` evidence before the colon, never as the real
  source, and a caller `critical` label never dresses a user record up as a
  system line.
* **Terminal-injection defence.** Every attacker-controlled string — the
  message, component, requested source, and string `data.*` values — is passed
  through an escaping writer that renders any control character (C0, `DEL`, or
  C1) as a visible `\xNN` and doubles a backslash, so caller text can never
  move the cursor, change colour, clear the screen, forge a prefix, or split
  itself across lines. The emitted line is control-byte-free regardless of
  input (asserted by the `fuzz_render` harness). Field names obey the
  `FieldName` grammar and non-string values render as control-free text, so
  both pass through unchanged.

## Journal ingress ABI and service (`rustos_abi::log_ingress`, `journald`)

An ordinary process never appends to a segment: it frames a request and posts
it to the journal service. The wire contract is `rustos_abi::log_ingress`
(`no_std`, allocation-free, fail-closed), a sibling of the `sysinfo` and
driver-store endpoint ABIs rather than part of the C-callable surface:

* `LOG_INGRESS_ENDPOINT` — the well-known synchronous call endpoint the journal
  service binds (unrestricted-sender: any process may write, since authority is
  the attested origin, not the transport).
* `LogIngressRequest` — a caller's message plus its *advisory* level and stream
  discriminants, an optional trusted-emitter subsystem label, its
  component/tag/event-id, the source it *requests*, and a flat set of `data.*`
  fields. The `data.*` pairs reuse the one shared named-field codec, so an
  ingress field and a persisted record field cannot drift. It carries **no**
  authoritative fact: origin, source, effective stream/level, sequences, and
  integrity hashes are all decided by the journal. The caller-field maxima are
  the single definition the persisted `record` model imports, so a request that
  validates always persists.
* `encode_reply` / `decode_reply` — a status-word reply: accepted, or the
  `Errno` the journal refused it with.

The `rustos-journald` crate (`userland/system/journald`) is the
architecture-neutral dispatch core over this ABI. `serve` decodes and fully
validates a request, resolves the advisory stream/level against the closed
vocabularies (fail-closed on an unknown discriminant), rate-limits the record
via `admit_limited` under the caller's kernel-attested `Origin` — never a
caller claim — and, for an admitted record, builds the `data.*` set (rejecting
any name outside the `FieldName` grammar, which structurally forbids reserved
prefixes), commits to the injected `Journal`, and calls `note_spoof` for any
spoof attempt; it then drains any matured rate-limit drops via `emit_rate_loss`.
`store` derives the pure `/System/Logs/<stream>/<id>.seg` segment path the
production filesystem sink uses. The `Run` binary binds the endpoint, reads
each caller's peer origin, installs the ingress rate-limit policy, and drives
this core over the real filesystem. Boot-ring import, retention, and
aggregation remain staged follow-ons (`plans/SYSLOG.md` §8.1/§10/§11/§15).

## Typed-field value model (`field`)

The typed field-value model is defined in `rustos-abi` (`rustos_abi::field`,
its ABI-schema home) and re-exported as `rustos_log::field` so both the
`log_emit` record (`rustos_abi::log`) and the RustOS system log
(`plans/SYSLOG.md`) schema share **one** definition — there is no second
string-only field encoding. It defines the closed set of value types a field
may hold and how a single value is named, validated, and encoded; it does
**not** define the framed record or on-disk segment format — that is the
journal service's job.

* `FieldName` — a validated caller field name. The grammar is the
  case-sensitive ASCII identifier `[a-z][a-z0-9_]{0,63}`. Because the `.`
  separator is not in the grammar, a caller name can never collide with a
  reserved journal namespace; `reserved_prefix` screens *qualified* names
  (`record.` / `origin.` / `source.` / `integrity.` / `sys.`) at the layer
  that can contain a dot.
* `FieldValue` — the *closed* value set: null, bool, signed/unsigned 64-bit
  integer, fixed-point `Decimal`, `Time64`, `Duration64`, bounded UTF-8 string,
  bounded bytes, `Uuid`, `IpAddr`, `MacAddr`, kernel error code, capability id
  (only the public numeric id — never a raw token), and a same-type bounded
  `FieldList` of scalars. Records are flat: nested maps and nested lists are
  forbidden so search, indexing, and rendering stay cheap. The scalar ABI types
  (`Time64`, `Duration64`, `Errno`, `CapabilityId`) are reused from `rustos-abi`
  so a logged value and its ABI form cannot drift apart.
* `FieldValue::encode` / `FieldValue::decode` — a compact little-endian codec
  for a single value (a tag byte plus payload). Decoding borrows variable-length
  data from the buffer, so the model stays allocation-free. Every length, tag,
  UTF-8, and range constraint is checked and fails closed with an `Errno`;
  nothing partial is written on an encode error. `encode_list` builds a list
  value from a slice of same-type scalar elements; `FieldList` iterates the
  decoded elements lazily.
* `ToFieldValue` — the only gate between application data and the log. A
  secret-bearing wrapper type (a key, password, or capability token)
  deliberately does **not** implement it, so a secret cannot be logged by
  construction: there is no blanket impl and no `Display`/`Debug` fallback.

This model and the audit `chain` above share one SHA-256 hash chain; the log
service layers its record and segment encoders on top of these values and that
chain, never a second value model or a second chain.

## Log attestation (`attest`)

The `attest` module supplies the two cryptographic foundations the system
log's tamper-evidence rests on, without building the journal or the on-disk
segment format (those are the log service's job):

* `machine_id_hash` / `stream_genesis` — derive a log stream's hash-chain
  genesis value, binding it to *this installation* (`machine_id_hash`), the
  stream, and the kernel's per-boot `BootId` through domain-separated
  SHA-256 over `lib/crypto`. A first segment chains to this value instead of
  the all-zero `GENESIS_ANCHOR`, so a segment lifted from another
  installation, stream, or boot fails verification. The genesis is not
  secret — every input is public and a verifier recomputes it freely;
  confidentiality rests on the seal below.
* `LogAttestationKey` — a per-installation secret (a 256-bit HMAC-SHA256 key)
  that `seal`s closed audit/security segments and `verify`s them in constant
  time. It is scrubbed from memory on drop, never implements `ToFieldValue`
  or any rendering trait (so it cannot be logged), and its raw bytes never
  leave the type — callers seal and verify *through* it. `to_file_bytes` /
  `from_file_bytes` are the on-disk key-file image the image builder /
  installer provisions under `/System/Security/Keys/`, gated by the inode
  owner/mode model (system-user-owned, `0o600`) until the journal principal
  exists — no capability is minted ahead of that holder.

All sealing uses the audited HMAC-SHA256 in `lib/crypto`; this module never
names an upstream crypto crate and never hand-rolls a primitive.

## What this crate is not

It is not a re-implementation of `tracing` or `log`. The API is small on
purpose: a single sink per process is wired up at boot and never
discovered, so there is no global registry, no dynamic dispatch beyond
the trait object the caller already holds.
