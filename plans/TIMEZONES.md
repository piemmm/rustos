# TIMEZONES.md — Civil time: zones, the IANA rules, and local rendering

Binding under `AGENTS.md`. This plan adds civil time zones to TAIRiX: the
compiled IANA rule store the system ships, the `no_std` engine that resolves an
absolute instant into a local reading (and back), the setting that says which
zone a machine and a user are in, and the conversion every surface that shows a
timestamp to a human renders through.

Nothing here changes what TAIRiX *stores*. Absolute time stays `Time64` UTC
everywhere — syscalls, IPC, logs, file metadata, on-disk formats (`§21`). A zone
is **presentation**, applied at the moment a human reads or types a time, and
never on the wire or on disk.

---

## 1. The defect

TAIRiX has no notion of a time zone at all. `tairix_fsmeta::calendar::CivilTime`
decomposes a `Time64` as UTC and every surface that shows a timestamp shows UTC:
`ls`'s date column, `stat`, `ps`, `top`, the log reader, the file manager's
properties sheet, the desktop clock, the login/lock clock, and `datetime.app`'s
fields. A user outside UTC therefore reads every time on the system an hour or
thirteen wrong, and — worse — *sets* the clock wrong, because `datetime.app`
takes the fields it is given as UTC.

Two consequences that make this a correctness defect rather than a nicety:

- **A wrong clock is a wrong system.** Certificate validity, log correlation,
  scheduled work, and `arxfs` timestamps are all read by humans who will
  "correct" a clock that is displaying UTC as if it were local, stepping the
  machine's actual UTC clock off by the zone offset.
- **The user cannot even express the truth.** There is no way to say "this
  machine is in Europe/London", so no surface can be right.

---

## 2. Design

### 2.1 The data is IANA's; the code is ours

Civil-time rules are facts about the world, published by the IANA time-zone
database. They cannot be derived, and hand-authoring them would be inventing
history. So the *data* is vendored from IANA and the *code* is entirely
first-party (`§2.12`): TAIRiX writes its own compiler and its own reader, and
depends on no external crate for either.

- **Vendored input:** `tools/tzcompile/data/tzdata.zi`, the single-file source
  form IANA distributes (`Rule` / `Zone` / `Link` lines). One file, ~110 KiB of
  text, pinned by its SHA-256 in the compiler; a mismatch fails the build closed
  (`§19.3` source-hash pinning). Its `# version` line is the release TAIRiX
  ships and is carried into the compiled store.
- **Not vendored:** IANA's compiled `TZif` binaries. Shipping someone else's
  build output when the authoritative source is 110 KiB of text would put a
  build step we do not control inside the trusted computing base.
- **Reference `TZif` files are test data only.** A small set of vendored
  reference binaries (`RFC 8536`) under `tools/tzcompile/tests/reference/`
  serves as an independent oracle: our compiler's transitions for those zones
  must agree with IANA's own build, transition for transition. They are never
  read at runtime and never shipped.

### 2.2 One compiled store, our own format

`tools/tzcompile` is a host-only Rust library plus a thin CLI. It resolves the
`Zone`/`Rule`/`Link` semantics — the `until` chain, `at` times in wall/standard/
universal form, `on` day rules (`lastSun`, `Sun>=8`, …), negative DST, the
pre-standard-time LMT era — and emits one compact document:

```
/System/Zoneinfo/zones.tzx        # every zone, its transitions, and its aliases
```

The format (`docs/src/lib/tz.md`, binding) is designed for a `no_std` reader on
untrusted-shaped bytes: a fixed header (magic, format version, IANA release,
counts, a `lib/crc32c` checksum over the body), explicit little-endian
integers (never a native-endian cast, `§23.2`), fixed-width records, a sorted
name index for binary search (`§27`), and a shared abbreviation string table.
Every zone carries:

- its **transition table** — `(at: i64 UTC seconds, offset: i32 seconds,
  abbrev, is_dst)`, so history is exact rather than extrapolated;
- its **trailing rule** — the two annual transitions in force after the last
  explicit one, so a date arbitrarily far in the future resolves correctly
  rather than freezing at the last table entry (`Time64` is 64-bit; `§21`).

Aliases (`Link`) resolve to a canonical zone in the index, so `Europe/Belfast`
and `Europe/London` are one zone with two names and one set of rules (`§2.2`).

The store is produced by `tools/mkimage` from the vendored source at image-build
time and planted read-only under `/System` — pure Rust, no shelling out
(`§12`), byte-reproducible from a pinned input (`§19.3`). It is not committed as
a binary artefact.

#### 2.2.1 The store's wire contract (binding)

The compiler writes it and `lib/tz` reads it, so it is fixed here once. Every
integer is little-endian and every read is a `from_le_bytes` over a slice —
never a pointer cast — so the format needs no alignment and behaves identically
on every target (`§23.2`). Offsets into the string section are byte offsets
from that section's start.

Header, 40 bytes:

| at | type | field |
|---|---|---|
| 0 | `[u8; 4]` | magic `TZX1` |
| 4 | `u16` | format version, `1` |
| 6 | `u16` | flags, `0` (any other value is refused) |
| 8 | `u32` | IANA release string offset (e.g. `2025b`) |
| 12 | `u32` | `zone_count` |
| 16 | `u32` | `name_count` (canonical names plus aliases) |
| 20 | `u32` | `transition_count` (all zones) |
| 24 | `u32` | `abbrev_count` |
| 28 | `u32` | `trailing_count` |
| 32 | `u32` | `strings_len` |
| 36 | `u32` | CRC-32C (`lib/crc32c`) over every byte after the header |

Then six sections, in this order, each packed with no padding:

1. **Names**, `name_count` × 8: `u32` name offset, `u32` zone index. Sorted
   strictly ascending by the name's UTF-8 bytes, so a lookup is a binary
   search; an alias is simply a name pointing at its canonical zone.
2. **Zones**, `zone_count` × 16: `u32` canonical-name offset, `u32` first
   transition index, `u32` transition count, `u32` trailing-rule index
   (`u32::MAX` when the last transition's offset continues for ever).
3. **Transitions**, `transition_count` × 16: `i64` UTC seconds the offset
   begins, `i32` offset east of UTC in seconds, `u16` abbreviation index,
   `u8` flags (bit 0 = daylight saving in force), `u8` zero. Strictly
   ascending within a zone, and a zone's **first** transition is `i64::MIN`
   so every instant resolves.
4. **Abbreviations**, `abbrev_count` × 4: `u32` string offset.
5. **Trailing rules**, `trailing_count` × 32: `i32` standard offset, `i32`
   daylight offset, `u16` standard abbreviation index, `u16` daylight
   abbreviation index, then two 8-byte annual specs (daylight start, then
   standard resumption): `u8` month `1..=12`, `u8` week `1..=5` (5 = last),
   `u8` weekday `0..=6` (0 = Sunday), `u8` zero, `i32` local wall seconds
   after midnight (may fall outside `0..86400`), padded to 32 with zeroes.
6. **Strings**, `strings_len` bytes of NUL-terminated UTF-8.

Fixed bounds, which are security bounds and do not scale (`§24.4`): the whole
store ≤ 4 MiB, a zone name ≤ 64 bytes, an abbreviation ≤ 16 bytes,
`transition_count` ≤ 2^20. A store that violates any structural rule above —
bad magic, unknown version, non-zero flags, a section that overruns the buffer,
an out-of-range offset or index, an unsorted name index, a non-ascending
transition, a string that is not NUL-terminated inside its section or is not
UTF-8, a checksum mismatch — is refused whole, with a typed error and no
partial view (`§5.4`).

### 2.3 `lib/tz` — the engine every consumer reads through

A `no_std` crate, the single definition of every zone question:

- `ZoneInfo::parse(&[u8])` — total, allocation-free validation of the store:
  bounds, ordering, checksum, and string-table integrity, returning a typed
  error and never panicking (`§2.9`). Fixed input bounds are security bounds
  and stay fixed (`§24.4`).
- `Zone::offset_at(Time64) -> ZoneOffset` — binary search of the transition
  table, then the trailing rule. `O(log n)`, no allocation (`§2.16`).
- `Zone::local(Time64) -> LocalTime` — the offset applied, then
  `tairix_fsmeta::calendar::CivilTime::from_time64` on the shifted instant.
  **The calendar arithmetic is not re-implemented**: there is one day/month
  decomposition in the tree and this reuses it (`§2.2`).
- `Zone::instant_from_local(fields, Disambiguation) -> Result<Time64, LocalError>`
  — the inverse, over `calendar::days_from_civil`. A local time in a
  spring-forward **gap** does not exist and a time in an autumn-back **overlap**
  names two instants: both are typed errors unless the caller states which it
  means. Guessing, clamping, or silently picking the earlier is forbidden
  (`§5.4`, `§21`'s no-silent-truncation rule applied to the same question).
- A fuzz harness over `parse` and over the resolvers (`§19.6`), and a proptest
  round-trip (`local → instant → local` is identity outside gaps).

The store is **system-shipped, read-only, integrity-checked data**, so the
reader is the in-tree `lib/fdt` / `lib/multiboot2` class — a total,
bounds-checked, fuzzed decoder — not a sandboxed third-party parser (`§19.5`).

### 2.4 Which zone: `TZ`, then the machine, then UTC

Resolution order, one definition in `lib/tz`, consulted by every consumer:

1. **`TZ` in the environment** — a canonical zone name. This is what a user's
   own zone rides on (login exports it from the account's settings) and what a
   script overrides per command; it is also what GNU tools honour, which
   `§16.7` requires TAIRiX's command apps to match. `TZ` is *data*: it selects
   a presentation and carries no authority (`§5.4`).
2. **The machine setting** — `/System/Settings/Timezone`, one canonical zone
   name (`#` comments, one setting, fail-closed grammar defined in `lib/tz`).
   It does not fit `lib/sysconfig`'s closed-value-set registry — a zone's value
   set is the store's index, not a hand-written list — so it is its own small
   document, exactly as `lib/netconfig` is for interfaces.
3. **UTC** — an unset, absent, malformed, or unknown-zone answer renders UTC and
   says so, rather than guessing a zone (`§5.4`). A machine with no store
   planted still shows every time; it shows it in UTC.

A name outside the store's index is refused at the point it is *set* (so a
mistyped zone is rejected while the user is looking at it) and treated as unset
when read (so a store that lost a zone degrades to UTC rather than to nothing).

No new capability is introduced (`§5.2`). Reading the store and the setting is
ordinary file access; writing the machine setting is the per-inode
owner/mode/ACL gate on `/System/Settings` under the writer's own attested
identity — which is why the zone is set through the same account elevation the
clock already uses.

### 2.5 One rendering seam, no second formatter

Every surface that shows a timestamp to a human renders through the one seam:
the process's resolved `Zone` (loaded once, lazily, by a `lib/rt` helper that
reads `TZ`/the machine setting/UTC and the store) and `Zone::local`. A program
does not read the store itself, does not re-derive the resolution order, and
does not carry its own offset arithmetic.

Where a GNU counterpart offers UTC explicitly (`ls --time-style`, `-u`), the
TAIRiX tool offers the same switch with the same spelling (`§16.7`).

---

## 3. Invariants

- Stored, compared, and transmitted time is UTC `Time64`, unchanged (`§21`).
- Exactly one calendar decomposition (`lib/fsmeta::calendar`), one zone engine
  (`lib/tz`), one resolution order, one store format (`§2.2`).
- No zone data is compiled into a binary: the store is a file the image builder
  plants and the engine reads (`§16.5`'s data-on-disk rule).
- A missing, truncated, corrupt, or unknown-zone answer renders UTC and states
  the fallback; nothing fabricates an offset (`§5.4`).
- A local time that does not exist, or exists twice, is a typed error the caller
  resolves explicitly — never guessed (`§5.4`).
- Zone resolution allocates nothing per timestamp and is `O(log n)`: a directory
  listing of ten thousand files resolves its zone once (`§2.16`).
- Platform-neutral throughout: no `cfg(target_arch)`, no board coupling
  (`§2.20`).

---

## 4. Stages

Each stage is complete on its own and ends on a whole-project-green gate
(`§2.15`).

### TZ1 — the vendored source and the compiler *(planned)*

`tools/tzcompile`: the vendored pinned `tzdata.zi`, the `Zone`/`Rule`/`Link`
resolver, the store writer, the CLI, and the reference-`TZif` oracle tests.
Adds `tools/tzcompile` to `AGENTS.md` §3.

Host tests: `Sun>=8`-style day rules; `at` in wall/standard/universal form;
negative DST (Europe/Dublin); a half-hour zone (Asia/Kolkata) and a 45-minute
one (Asia/Kathmandu); a 30-minute DST step (Australia/Lord_Howe); a southern-
hemisphere zone; the LMT era before standard time; a zone that has changed its
rules more than once; every canonical zone's transitions matched against the
vendored reference binaries.

### TZ2 — `lib/tz`, the engine *(planned)*

The crate above: parse, `offset_at`, `local`, `instant_from_local`, the
`TZ`/machine/UTC resolution order, the `/System/Settings/Timezone` grammar, the
fuzz harness, and the proptest round-trip. Adds `lib/tz` to `AGENTS.md` §3 and
its `docs/src/lib/tz.md` page (including the binding format spec).

Host tests: dates before 1970 and after 2038 (`§21`); the gap and overlap cases
in both hemispheres; a truncated, checksum-broken, and mis-ordered store all
refused; an unknown zone name refused at set and UTC at read.

### TZ3 — shipping the store *(planned)*

`tools/mkimage` compiles and plants `/System/Zoneinfo/zones.tzx`; the charter's
§16.2 store list gains `Zoneinfo/` (see `PLAN.md`'s charter-amendment log). The
QEMU image fixtures plant it the same way, so a guest test can resolve a zone.

### TZ4 — the rendering seam *(planned)*

The `lib/rt` lazy per-process zone (`TZ` → machine → UTC) and the one
`Zone::local` rendering every consumer calls. No consumer reads the store
directly.

### TZ5 — the desktop clock *(planned)*

The taskbar clock, the login and lock clocks, and the clock menu's heading
render local and name the zone. The unset placeholder is unchanged
(`taskbar::clock::UNSET_LABEL`).

### TZ6 — setting the zone *(planned)*

`datetime.app` gains the zone chooser (the store's index, searchable) beside its
date and time fields, composes the instant it commits *from local fields through
`instant_from_local`*, and writes `/System/Settings/Timezone` under the elevated
account the clock menu already authenticates. Login exports the user's own zone
as `TZ`, so two users on one machine can read their own local time (`§26.2`).

### TZ7 — every timestamp a user reads *(planned)*

`ls`, `stat`, `ps`, `top`, the log reader, `sysmon`, and the file manager's
properties sheet render local through the TZ4 seam and honour the GNU UTC
switches (`§16.7`). Each carries its own regression test with a fixed zone and
a fixed instant.

### TZ8 — docs and the matrix *(planned)*

`README.md`'s feature matrix, the `docs/src/` pages the stages touched, and this
plan's own jump-sheet row (`AGENTS.md` §15.18).

---

## 5. Non-goals

- **A `date` command.** TAIRiX ships no `date` app today; adding one is its own
  feature (`plans/APPS.md`), not part of the zone subsystem. When it lands it
  renders through TZ4 like every other tool.
- **A network time client (NTP/NTS).** Correcting the *instant* is a separate
  subsystem from naming the *zone*; `datetime.app` and the clock's provenance
  states (`WallTimeState`) already cover the human-set case.
- **Leap seconds.** TAIRiX time is UTC-with-a-linear-count (`Time64`), as
  everywhere else in the tree; the IANA `leapseconds` file is not compiled in
  and TAI is not modelled. Stated so it is a decision, not an omission.
- **Per-program zone databases.** There is one store and one engine; a program
  that wants its own is a `§2.2` defect.
