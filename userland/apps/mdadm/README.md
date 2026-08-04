# `tairix-mdadm` — inspect and administer RAID arrays

The administrator's array tool (`plans/FIX-IO.md` IO6), shipped as the
system command store bundle `mdadm.app` so the shell resolves the bare word
`mdadm` to it. The option surface tracks the reference `mdadm` — `-C`/
`--create`, `-D`/`--detail`, `-E`/`--examine`, `-a`/`--add`,
`-r`/`--remove`, `-S`/`--stop`, with `-l`/`--level`,
`-n`/`--raid-devices`, `-c`/`--chunk`, `--`, `-h`/`-?`/`--help`, and
`-V`/`--version` — so a user who knows that tool finds this one
familiar. Exactly one mode is accepted per invocation; the short help
renders from the bundle's own on-disk `Help/` tree through the shared
`lib/help` engine (`plans/APPS.md` §4), never from text compiled into
the binary.

The inventory is a read: `tairix_procinfo::raid_arrays` and
`tairix_procinfo::raid_members` issue the System Information queries the
array composer answers, at the same `CAP_SYSINFO_HW` bar the hardware
tree is read under. The mutations are a posted control frame: a
`tairix_abi::raid_admin::RaidControlOp` encoded to
`RAID_CONTROL_ENDPOINT`, with the reply decoded through
`tairix_abi::reply::decode_status_reply` — or
`raid_admin::decode_create_reply`, which carries the identity the
composer minted for a new array. The composer is the policy point: it
checks the caller holds `CAP_STORAGE_ADMIN` against the kernel-attested
origin, so this tool tests no authority of its own and holds none; it
reports the refusal it is given.

Two operand spellings are TAIRiX's own, because there is no `/dev`, and
both fail closed rather than guess. A **device** is its hardware-tree
node id, spelled `node:<id>` — the same name the reports print. An
**array** is its 128-bit identity as 32 lower-case hexadecimal digits;
the full identity resolves, and so does any prefix naming exactly one
live array, while a prefix matching more than one is refused as
ambiguous and one matching none is refused as unknown. A `--create`
additionally refuses a device named twice or a member set larger than
an array can hold before it posts anything, so the diagnostic names the
offending operand. The composed levels are 0, 1, 5, 6, 10, and triple
parity; there is no RAID4, so `--level=4` is refused with that reason.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary and the shared `tairix-help` and
`tairix-procinfo` crates. The parser (`src/command.rs`), the name
resolution (`src/resolve.rs`), the report rendering (`src/render.rs`),
and the engine (`src/client.rs`) are pure and host-tested over injected
reader, controller, and output seams; the freestanding `Run` binary
(`src/run.rs`) binds the production seams over `tairix-rt` and the
inherited standard streams. Concise advisory records go to fd 3 where
they add context the report does not carry — a reduced-redundancy
summary, the blank devices the array view omits, and the empty-machine
note — and never change the primary output.

`cargo test -p tairix-mdadm` drives the parser (every option, every
refusal, `--`, help/version precedence), the resolver (node names, full
and partial identities, ambiguity, duplicate and oversized member
sets), the renderers (an optimal array, a degraded array with an absent
slot, a rebuild in progress, an empty machine, a blank-device listing),
the engine (each mode's request and rendering, a denied read, a denied
mutation, a composer refusal, an unresolved name, each advisory), and
the help-document switch pinning across every required locale.
