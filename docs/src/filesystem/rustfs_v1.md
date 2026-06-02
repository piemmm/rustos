# RustFS v1 specification (next generation)

This page is the documentation-area home of the **RustFS v1** on-disk
design: the next-generation native RustOS filesystem that supersedes the
shipping [rustfs driver](./rustfs.md). It is a copy-on-write, **always
encrypted**, checksummed, compressed, deduplicating, SSD-aware and
recoverable filesystem. The authoritative implementation spec is
`.junie/RUSTFS.md`; this page mirrors it for the book so the design is
covered by `cargo xtask docs-check`. Where the two disagree, both are
subordinate to `AGENTS.md`.

RustFS v1 is being delivered **in stages** (see
[Implementation order](#implementation-order)); each stage lands behind
the same frozen `FilesystemRead` / `FilesystemWrite` / `FilesystemSecurity`
/ `FilesystemTimestamps` traits the current driver already implements, so
the [VFS](./overview.md) policy layer is unaffected by the format change.
Until a stage lands, the shipping [rustfs driver](./rustfs.md) remains the
native filesystem.

## One mandatory profile

RustFS has **one production profile**. Copy-on-write, transactions,
metadata and data integrity, redundancy, encryption, compression,
deduplication, shared extents, TRIM, health monitoring, scrub, and
check/rescue are **always on** and are **not** tunable by mkfs option,
mount option, ioctl, per-volume or per-file policy, build feature,
environment variable, or userland configuration. There is no
`compression=off`, `dedupe=off`, `checksums=off`, `encryption=off`,
`trim=off`, `scrub=off`, `metadata_copies=1`, or plaintext mode.

The normal RustOS mount security flags (`ro`, `noexec`, `nosuid`, `nodev`)
remain valid: they are permission policy (`AGENTS.md` §5.3 / §16), not
RustFS feature configuration.

## Mandatory features

| Area | Mandatory RustFS v1 behaviour |
|---|---|
| COW | No committed metadata block is overwritten in place. |
| Transactions | Mount the highest valid committed root after ordinary power loss. |
| Metadata integrity | Every metadata block is self-identifying and authenticated/checksummed. |
| Data integrity | Every physical record is checksummed; every logical record has a strong hash. |
| Redundancy | Critical metadata has at least two physical copies. |
| Encryption | Every volume is encrypted; plaintext RustFS is forbidden. |
| Compression | First-party RustOS zstd-fast-style compression is always active. |
| Deduplication | Exact, byte-verified dedupe; it may miss duplicates but never merges unequal bytes. |
| Shared extents | Reflink / shared immutable chunks are core storage. |
| TRIM | mkfs discards the target range when supported; mounts trim safely in batches. |
| Health | SMART/NVMe snapshots are stored and used when exposed by the storage stack. |
| Scrub | Online verification and repair from redundant copies. |
| Check / rescue | Offline structural validation, repair, index rebuild, and damaged-volume extraction. |
| Time | All persistent timestamps use RustOS `Time64` (`AGENTS.md` §21). |
| Security | POSIX bits + ACLs + capability gates on every inode (`AGENTS.md` §5.3). |

## On-disk model

RustFS stores immutable physical records referenced by logical extents.
The authoritative trees descend from a superblock ring of recent
transaction roots:

- superblock ring and root history;
- inode tree;
- extent tree;
- chunk table, refcount tree, and reverse-reference tree.

The free-space tree, dedupe index, directory acceleration indexes, health
summaries, scrub progress, and allocation heat maps are **rebuildable**: a
corrupt rebuildable tree must never make a valid volume unmountable, and
`rustfs check` rebuilds it from the authoritative metadata.

## Fixed v1 constants

The v1 constants are global and not user-tunable: a 16 KiB metadata block
target, a 128 KiB normal data-record target (256 KiB for large sequential
writes), inline/packed small files, a BLAKE3-256 logical hash and a keyed
metadata authenticator (both through [`rustos-crypto`](../lib/crypto.md)),
a fast physical checksum selected by the storage ABI, at least two copies
of critical metadata, and retained root history for rollback. A future
format revision may change these globally; a mounted v1 volume exposes no
runtime control over them.

## Write and read pipeline

The write path is: logical hash → same-encryption-domain dedupe lookup →
byte-verify the candidate (or continue as unique) → first-party zstd-fast
compression attempt (store raw if it does not win) → encrypt the stored
representation → checksum/authenticate the metadata and physical record →
write the new physical record → commit the new COW root. The read path
reverses it: verify the physical checksum → decrypt → decompress if
compressed → verify the logical hash → return plaintext.

Dedupe runs before compression, compression before encryption. Dedupe
need not be exhaustive in the foreground; it requires bounded foreground
discovery, background discovery, exact byte-verification before sharing,
and rebuildable dedupe metadata.

## Encryption

Every volume is encrypted; `mkfs.rustfs` fails closed if no valid key
source is supplied by the installer, recovery flow, or storage-policy
service. Data, filenames, directory entries, and sensitive metadata are
encrypted; only the minimal unlock/discovery header may remain plaintext.
Primitives come from [`rustos-crypto`](../lib/crypto.md) only — RustFS
hand-rolls no encryption or authentication primitive (`AGENTS.md` §2.12).
AES-256-XTS is preferred where hardware acceleration is available, with
Adiantum or another approved wide-block mode selected automatically when
AES is unsuitable; the selection is automatic and not user-tunable.
Deduplication is permitted only within one encryption domain, and keys are
never stored unwrapped on disk. The key hierarchy is a volume wrapping key
over a domain key over the content, filename, metadata-authentication, and
dedupe-domain keys. Secret-holding allocations inherit the RustOS
zero-on-free requirement (`AGENTS.md` §4).

## Integrity, scrub, check, and rescue

Each metadata block carries its magic, block type, format version,
filesystem UUID, owner object, generation, logical and physical address,
payload length, and an authenticator/checksum over identity, owner,
generation, expected address, and payload — enough to detect stale,
misdirected, wrong-type, torn, or bit-rotted blocks. On a verification
failure RustFS tries redundant copies, repairs bad copies from good ones,
returns an error when no valid copy exists, and records the affected
inode/range for health, scrub, and check.

Three tools operate the integrity machinery:

- `rustfs scrub` — online verification and repair from redundant copies;
  resumable and safe to interrupt.
- `rustfs check` — offline structural validation and repair (superblocks,
  root history, trees, inodes, extents, refcounts, reverse refs,
  directories, ACL/capability metadata) plus free-space and dedupe-index
  rebuild.
- `rustfs rescue` — damaged-volume root discovery and best-effort file
  extraction without a fully mountable filesystem.

## Crash consistency

The commit order is: allocate records → write data records → write
metadata leaves → write metadata internal blocks → write the new root →
write the commit record → flush → publish the superblock slot → flush.
After power loss the mount selects the highest valid committed root and
ignores a partial transaction; an ordinary crash needs no full
`rustfs check`. Discard may never destroy data reachable from a retained
root, snapshot, reflink, deduped extent, or recovery root.

## Dependency rules

The driver lives at `drivers/filesystem/rustfs/`. Shared ABI types belong
in `lib/abi`; cryptographic primitives and key handling go through
[`rustos-crypto`](../lib/crypto.md); RustFS never links against kernel
internals. **No external zstd/compression dependency is permitted** — the
zstd-compatible codec subset is written in the RustOS workspace in Rust
(`AGENTS.md` §2.12). It lives under `drivers/filesystem/rustfs/src/` while
only RustFS uses it; if another crate needs it, it is promoted to a
first-party `lib/compress` crate with `AGENTS.md`, `PLAN.md`, docs, tests,
and CI updated in the same change.

## Implementation order

RustFS v1 is built bottom-up so each stage rests on solid foundations:

1. On-disk headers, superblock ring, transaction roots.
2. COW metadata trees (inode, extent) and free-space rebuild.
3. Metadata authentication/checksums and duplicated critical metadata.
4. Encrypted volume creation, key hierarchy, filename/data encryption.
5. Data records with physical checksum and logical hash.
6. First-party RustOS zstd codec and its RustFS integration.
7. Chunk table, refcounts, reverse refs, reflinks, dedupe index.
8. Online scrub.
9. Offline check and rescue.
10. TRIM/discard queues and mkfs-time discard.
11. Device-health baselines and health-triggered scrub.
12. Fuzz, proptest, crash-replay, and corruption-injection suites.

Dedupe is not implemented before COW, checksums, refcounts, and
check/rebuild are solid. The staged delivery and the live next-session
prompt are tracked in `.junie/RUSTFS.md` and `.junie/next-rustfs-prompt.md`.
