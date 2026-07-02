# System-log tool (`log`)

`log` (`userland/shell/log`, crate `rustos-logtool`) is the terminal reader,
renderer, and verifier of the RustOS system log. The system log is a set of
immutable, append-only, hash-chained segment files under
`/System/Logs/<stream>/`, one directory per stream (`boot`, `runtime`,
`debug`, `security`, `audit`, `journal`). `log` reads those files and turns
them into readable output, or checks their integrity. There is no `/proc` or
`/sys`: the authoritative data is the segment files and the RustOS APIs.

## Commands

| Command | Purpose | Default format | Allowed `--format` |
|---------|---------|----------------|--------------------|
| `log show [stream]` | render records | line | line, json, md, table |
| `log report [stream]` | render a human report | md | md, table |
| `log export [stream]` | export structured records | json | json |
| `log verify [stream]` | verify hashes, chains, and seals | — | — |
| `log help` | usage banner | — | — |

A `stream` operand selects one stream; omitting it selects every stream,
oldest records first. Rendered records go to stdout; verification results and
diagnostics go to stderr, and command metadata to `stdinfo` — the tool binds
only to its inherited standard streams, never a device.

## What it reuses

The tool is a read/render/verify state machine over the one system-log model
in [`rustos-log`](../lib/log.md): the segment reader, the record decoder and
its per-segment dictionary view, the boot line renderer, the JSON/Markdown/
table renderers, and the segment verifier. It re-implements none of the
on-disk format. The two operations that touch the outside world — reading a
stream's segments and writing the terminal — are object-safe seams
(`SegmentSource`, `Output`), so the whole engine is exercised by host tests
with in-memory fixtures and no kernel.

## Security

- **Provenance is preserved, never obeyed.** System-attested facts (stream,
  sequence, CPU, monotonic/wall time, effective level, the system-derived
  source, the attested origin) are separated from caller content; a caller's
  *requested* privileged source or stream is shown inertly as a claim, never
  promoted to the real one.
- **Caller text can never forge output.** Control characters and quotes in
  caller-controlled strings are escaped by the renderers, so a hostile record
  cannot move the cursor, forge a prefix, or break the JSON.
- **Verification fails closed.** A corrupt, truncated, or tampered segment is
  reported and yields a non-zero result. The `audit` and `security` streams
  are sealed with a MAC; `log verify` of those streams cannot succeed without
  the per-installation log-attestation key rather than passing them unchecked.

## Status

The host-testable render/verify library lands first, following the `cat` / `ls`
precedent. The freestanding `Run` binary — wiring the real `/System/Logs`
reader over the `rustos-rt` filesystem syscalls and the standard streams — and
its QEMU integration vertical are the next sub-increment.
