# Kernel security subsystem

`kernel/sec` is the in-kernel home of every datum a privileged operation
must consult: the user/group identity tables, the per-task capability
tables, the manifest verifier that gates binary loading, and the audit
log writer that records every security-relevant decision.

It depends only on `kernel/sync`, `kernel/mem`, and the shared
`lib/abi`, `lib/caps`, `lib/crypto`, and `lib/log` crates (Stage 2.4
brief). Filesystem ACLs, IPC dispatch checks, and syscall plumbing live
in later stages and consume this crate's public API rather than
re-implementing it.

## Identity model

Every process runs as `(uid, gid, supplementary_gids, capability_set)`.
The kernel stores these tuples in an `IdentityTable` built by an
`IdentityTableBuilder` (see the rustdoc on `kernel/sec`):

- `UserRecord { uid, primary_gid, supplementary_gids, capability_grants }`
- `GroupRecord { gid }`

The builder verifies, in one pass, that:

| Check                                  | Failure                |
| -------------------------------------- | ---------------------- |
| No two users share a `uid`             | `Errno::BadMagic`      |
| No two groups share a `gid`            | `Errno::BadMagic`      |
| Every referenced group exists          | `Errno::NotFound`      |
| `supplementary_gids.len() ≤ MAX_SUPPLEMENTARY_GROUPS` | `Errno::LengthOutOfRange` |

Loading an on-disk record set is the responsibility of userland (see
`AGENTS.md` §5.1); `kernel/sec` accepts already-parsed records and
freezes them.

### No ambient authority

`uid == 0` confers **no** privilege in `kernel/sec`. Authority is the
capability set attached to the record, never the numeric id. The unit
tests `identity::uid_zero_is_not_ambient_root` and
`captable::uid_zero_gets_no_extra_powers` lock this invariant in; any
change that lets the kernel branch on `uid == 0` must be rejected by
review.

## Capability flow

```
                         signed manifest                user grant
                              │                              │
                              ▼                              ▼
                      verify_manifest()            IdentityTable::user()
                              │                              │
                              └──────────────┬───────────────┘
                                             ▼
                            TaskCapabilities::derive()  ← intersection
                                             │
                          delegate()  ◄──────┼──────►  apply_token()
                                             │
                                         revoke()
```

`TaskCapabilities::derive(user_grant, manifest_request)` returns the
intersection of its two inputs. `delegate` and `apply_token` forward to
`lib/caps`, the single source of truth for the subset-only delegation
invariant; both replace the effective set with the (necessarily
narrower) result. `revoke` removes a single capability and is idempotent.

The lib-level property test in `lib/caps` asserts that a delegated set
is always a subset of its parent; the task-level analogue lives in
`kernel/sec/tests/proptest_invariants.rs` (Stage 2.4 brief).

## Manifest format

An `rxe` binary carries a fixed-size `ManifestHeader` (`lib/abi`)
followed by a body listing the `CapabilityId`s the binary wants to
exercise. The signed bytes are
`header[ManifestHeader::signed_range()] ∥ body`. The body is a tightly
packed array of little-endian `u16` capability IDs.

`verify_manifest` refuses a binary when **any** of the following hold:

| Outcome                                  | `Errno`                  | Audit event                        |
| ---------------------------------------- | ------------------------ | ---------------------------------- |
| Buffer too short / bad magic / oversize  | `BufferTooSmall`/`BadMagic`/`LengthOutOfRange` | `ManifestBadHeader`           |
| Header's `abi_version` ≠ current         | `AbiVersionUnsupported`  | `ManifestAbiMismatch`              |
| Body contains an unknown capability ID   | `OutOfRange`             | `ManifestUnknownCapability`        |
| `signer_pubkey` ≠ kernel's authority key | `SignatureInvalid`       | `ManifestSignatureInvalid`         |
| Ed25519 signature does not verify        | `SignatureInvalid`       | `ManifestSignatureInvalid`         |

A successful verification emits exactly one `ManifestVerified` event
and returns a `VerifiedManifest` carrying the verified request set.

The verifier deliberately does **not** check the header's
`syscall_table_hash` field; that lives in `kernel/syscall` (Stage 2.7 in
the issue brief's numbering), which can re-read it from the verified
manifest at dispatch time without re-parsing.

## Audit log writer

Every decision routes through `kernel/sec::audit::record`, which emits a
structured `rustos_log::Event` with a stable `EventId`. The
`kernel/sec` range is `1_000..2_000`; the issued identifiers are part
of the audit contract with external log consumers and may not be
re-used or re-numbered.

| ID   | Level | Name                              | When                                                                  |
| ---: | ----- | --------------------------------- | --------------------------------------------------------------------- |
| 1000 | Info  | `IdentityTableLoaded`             | Builder produced a verified `IdentityTable`.                          |
| 1001 | Error | `IdentityTableRejected`           | Builder rejected (duplicate id, unknown gid, oversize set).           |
| 1010 | Info  | `ManifestVerified`                | Signed manifest passed every check.                                   |
| 1011 | Error | `ManifestBadHeader`               | Manifest header malformed (bad magic, short buffer, oversize).        |
| 1012 | Error | `ManifestAbiMismatch`             | Header parsed but `abi_version` is not the current ABI version.       |
| 1013 | Error | `ManifestSignatureInvalid`        | Ed25519 verification failed, or `signer_pubkey` did not match.        |
| 1014 | Error | `ManifestUnknownCapability`       | Manifest body requested a capability ID the kernel does not know.     |
| 1020 | Info  | `TaskCapabilitiesDerived`         | Per-task set derived from user grant ∩ manifest request.              |
| 1021 | Info  | `TaskCapabilitiesDelegated`       | A delegated subset (or signed `CapabilityToken`) was installed.       |
| 1022 | Error | `TaskCapabilitiesDelegateWiden`   | A delegation attempt that would have widened authority was refused.  |
| 1023 | Info  | `TaskCapabilitiesRevoked`         | One or more capabilities were revoked from a task.                    |
| 1030 | Info  | `DmaAllocated`                    | DMA buffer granted to a task holding `CAP_MEM_DMA` (`AGENTS.md` §4).  |
| 1031 | Error | `DmaAllocDenied`                  | DMA allocation refused because the caller lacks `CAP_MEM_DMA`.        |
| 1040 | Info  | `MmioMapped`                      | Device register window mapped for a task holding `CAP_MMIO_MAP` (`AGENTS.md` §4). |
| 1041 | Error | `MmioMapDenied`                   | MMIO-map request refused because the caller lacks `CAP_MMIO_MAP`.     |

Adding a new event requires assigning the next free identifier in
`kernel/sec/src/audit.rs` and appending a row to this table in the same
commit (`AGENTS.md` §13).

### Per-decision invariant

The unit tests in `kernel/sec/src/{identity,manifest,captable}.rs`
exercise every code path that emits an audit event and assert the
**exactly-one-event-per-decision** rule (`AGENTS.md` §5.4.4): the
recorded id list, after one operation, has length 1 (or 2 when the
decision involves a prior `TaskCapabilitiesDerived` setup). This pins
the audit trail to the documented IDs above.

Rustdoc is the canonical reference for every type named above; build
it with `cargo doc -p rustos-kernel-sec --no-deps` and follow the
module index.
