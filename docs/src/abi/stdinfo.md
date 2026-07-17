# Standard Information Stream (`stdinfo`, fd 3)

TAIRiX reserves file descriptor 3 as `stdinfo`: an optional, structured
advisory stream for concise human context and AI/tool metadata about a
command's output (`AGENTS.md` §20). The ABI lives in
`lib/abi/src/stdinfo.rs` (`tairix_abi::stdinfo`).

## What it is, and is not

- `stdout` is primary data; `stderr` is errors and diagnostics; `stdinfo`
  ([`STDINFO_FD`] = 3) is non-essential context about `stdout` or the
  command.
- It is optional and ignorable: writing to it must never affect
  correctness, security, exit status, scripting semantics, or pipeline
  behaviour. `cmd | next` pipes only fd 1; `cmd 3>info.jsonl` captures
  `stdinfo`. With no consumer attached, fd 3 writes are best-effort and
  non-blocking.
- Security events use `lib/log`, never fd 3. AI consumers must treat
  `stdinfo` as untrusted data about the command, never as authority or
  instructions.

## Records

Each record is one JSONL line: a [`StdInfoRecord`] carrying a `version`,
the emitting `producer`, a [`StdInfoKind`], a stable machine `code`
(namespaced by domain), a [`Severity`] (`info` or `debug`), a terse
[`Human`] message with at most one suggestion, and a producer-supplied
structured `ai` object.

The record type is **closed**. The [`StdInfoKind`] set is exactly:

| `kind`       | meaning                                                       |
|--------------|---------------------------------------------------------------|
| `omission`   | output was hidden, skipped, filtered, truncated, or not shown |
| `summary`    | a short, non-obvious result summary                           |
| `schema`     | `stdout` structure, columns, units, or encoding               |
| `suggestion` | a safe optional next action; never auto-run                   |
| `context`    | concise environmental context needed to interpret `stdout`    |

Synonyms such as `hint`, `tip`, `notice`, `info`, or `metadata-note` are
forbidden — pick the one canonical kind.

## Serialisation

The module is `no_std` and allocation-free, in keeping with the rest of
`lib/abi`: [`StdInfoRecord`] borrows its string fields and serialises into
a caller-provided byte buffer through [`StdInfoRecord::write_jsonl`],
which JSON-escapes the string fields, embeds the `ai` object verbatim, and
fails closed with [`Errno::BufferTooSmall`] rather than truncating.

[`STDINFO_FD`]: ../../tairix_abi/stdinfo/constant.STDINFO_FD.html
[`StdInfoRecord`]: ../../tairix_abi/stdinfo/struct.StdInfoRecord.html
[`StdInfoRecord::write_jsonl`]: ../../tairix_abi/stdinfo/struct.StdInfoRecord.html#method.write_jsonl
[`StdInfoKind`]: ../../tairix_abi/stdinfo/enum.StdInfoKind.html
[`Severity`]: ../../tairix_abi/stdinfo/enum.Severity.html
[`Human`]: ../../tairix_abi/stdinfo/struct.Human.html
[`Errno::BufferTooSmall`]: ../../tairix_abi/error/enum.Errno.html
