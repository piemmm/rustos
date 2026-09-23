# tairix-ssh

The SSH protocol engine (`plans/SSH.md`). Stability tier: **experimental**.

Pure: no I/O, no clock, no randomness, so it runs unchanged inside the
capability-empty sandboxed worker `plans/SSH.md` §1.1 serves each connection
from, and the code the tests and fuzz harnesses (`fuzz_ssh_wire`,
`fuzz_ssh_ident`, `fuzz_ssh_packet`) exercise is the code that runs. Random
padding arrives as bytes from the host's CSPRNG; time-driven events arrive as
calls.

## Contents

- `wire` — the RFC 4251 §5 data types, decoded canonically and encoded
  exactly.
- `msg` — the transport's message numbers and `DisconnectReason`.
- `ident` — the RFC 4253 §4.2 identification exchange.
- `algorithm` — the ciphers and MACs `plans/SSH.md` §4 admits
  (`chacha20-poly1305@openssh.com`, `aes{128,256}-gcm@openssh.com`,
  `aes{128,192,256}-ctr` with `hmac-sha2-{256,512}` and their `-etm` forms)
  and one direction's zeroizing key material.
- `packet` — the binary packet protocol under every one of those framings,
  with the RFC 4344 rekey thresholds.
- `transport` — the transport layer's state machine: phase rules, strict key
  exchange (CVE-2023-48795), the key switch at `SSH_MSG_NEWKEYS`, and the held
  queues and padding reserve that let it wait rather than fail.

Key exchange, keys and certificates, authentication, and the connection
protocol are the later increments of `plans/SSH.md`.

Every cryptographic primitive is `lib/crypto`'s; this crate names none. See
`docs/src/lib/ssh.md` for how to drive it and `docs/src/security/ssh.md` for
what it defends against.

## Stability tier

`experimental` — the first increment of `plans/SSH.md`; the interface grows
with the key exchange above it. `no_std` + `alloc`, `#![forbid(unsafe_code)]`,
and no `unwrap`/`expect`/`panic!` outside tests: every input is answered with
a typed result.
