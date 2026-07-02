# rustos-logtool — the `log` CLI

Stability tier: **experimental**

`log` is the terminal reader, renderer, and verifier of the RustOS system log
(SYSLOG §14). The system log is a set of immutable, append-only, hash-chained
segment files under `/System/Logs/<stream>/`, one directory per stream
(`boot`, `runtime`, `debug`, `security`, `audit`, `journal`). `log` reads those
files and turns them into readable output, or checks their integrity. There is
no `/proc` or `/sys`: the authoritative data is the segment files and the
RustOS APIs.

## Commands

```text
log show    [stream] [--format line|json|md|table]   render records (default: line)
log report  [stream] [--format md|table]             render a human report (default: md)
log export  [stream] [--format json]                 export structured records (default: json)
log verify  [stream]                                 verify hashes, chains, and seals
log help                                              usage banner
```

A `stream` operand selects one stream; omitting it selects every stream, oldest
records first. Records are rendered to stdout; verification results go to
stderr.

## Design

The crate is a **read/render/verify state machine**, not a data source. It
consumes the one system-log model in `lib/log` — the segment reader, the record
decoder and its per-segment dictionary view, the boot/rich renderers, and the
segment verifier — rather than re-implementing the on-disk format. The two
operations that touch the outside world are behind object-safe seams:

- `SegmentSource` — read a stream's segments, one image at a time, oldest
  first, so a stream of any length streams through bounded memory.
- `Output` — write rendered bytes to the terminal.

This mirrors the seam discipline of the other userland tools (`cat`'s
`FileSource`, `ls`'s `Listing`, `sysinfo`'s `Transport`) and keeps every
parsing, rendering, and verification decision testable with in-memory fixtures
and no kernel.

## Security

- **Provenance is preserved, never obeyed.** System-attested facts are kept
  separate from caller content; a caller's *requested* privileged
  source/stream is shown inertly as a claim, never promoted to the real one.
- **Caller text can never forge output.** Control characters and quotes in
  caller-controlled strings are escaped by the renderers.
- **Verification fails closed.** A corrupt or tampered segment is reported and
  yields a non-zero result; a sealed `audit`/`security` stream cannot be
  verified without the log-attestation key rather than passing unchecked
  (SYSLOG §13).

## Status

The host-testable render/verify library lands here first, following the `cat`
/ `ls` precedent. The freestanding `Run` binary (wiring the real
`/System/Logs` reader over the `rustos-rt` filesystem syscalls and the standard
streams) and its QEMU integration vertical are the next sub-increment.
