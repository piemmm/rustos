# SSH security posture

`plans/SSH.md` is the design: a capability-empty protocol engine, a monitor
that never parses and holds no signing oracle, and `authorized_keys` reached
through a per-inode grant rather than a "read every user's files" authority.
This page records what the pieces landed so far defend against, and how. The
transport layer is the first of them; the engine is [`tairix-ssh`](../lib/ssh.md).

## Transport layer: threat model ↔ defence

| Threat | Defence |
|---|---|
| Prefix truncation (CVE-2023-48795, "Terrapin"): a message inserted during the unencrypted exchange and the first encrypted one deleted, leaving both sequence numbers in step | Strict key exchange. Once settled, the peer's `SSH_MSG_KEXINIT` must have been the first packet it sent, anything but the exchange's own messages before its first `SSH_MSG_NEWKEYS` — `SSH_MSG_IGNORE` and `SSH_MSG_DEBUG` included — ends the connection, and both sequence numbers restart at zero after every `SSH_MSG_NEWKEYS`. The packet after the first `SSH_MSG_KEXINIT` is not opened until strictness is settled, so nothing slips past the rule while it is undecided. Against a peer without it the rules of RFC 4253 still apply; which algorithms such a peer may be offered is the negotiation's decision. |
| Forged or altered packets | Every packet after the first `SSH_MSG_NEWKEYS` is authenticated, and tags are compared in constant time. Under `aes*-gcm`, `chacha20-poly1305`, and every `-etm` MAC the tag is verified before a byte is decrypted, so a forgery yields no plaintext at all; a failure ends the connection with `SSH_DISCONNECT_MAC_ERROR`. |
| Replay, reordering, and deletion of packets | The sequence number is in every MAC input and every `chacha20-poly1305` nonce, and the GCM invocation counter advances per packet, so a packet opened out of place fails to authenticate. |
| Nonce or MAC-input reuse from an exhausted sequence space | A key is never asked to carry a 2³²nd packet: sealing and opening both refuse it, fail closed. A rekey falls due long before, at 2³¹ packets or the cipher's RFC 4344 byte bound. |
| Resource exhaustion through declared sizes | `packet_length` is bounded at 256 KiB and must be aligned to the framing's block; it is judged the moment it is readable, before anything behind it is buffered. Identification lines are bounded at 255 bytes, a server's banner lines at 8 KiB and 1 024 of them. None of these bounds can be moved by anything a peer sends. |
| Weak algorithms | Absent rather than disabled: `plans/SSH.md` §4's refused set — SHA-1 and MD5 MACs, CBC, `arcfour`, the `none` cipher — has no implementation in the engine to be negotiated into. |
| Plaintext and keys left in memory | Session keys and every derived key live in types that wipe themselves when a direction rekeys or the engine is dropped. The transport's buffers wipe storage before handing it back to the allocator, on growth as well as on drop, so decrypted traffic never reaches freed memory. |
| Predictable padding, or an engine that must read a clock or an RNG | The engine has neither. Encrypted packets are padded from a reserve the host fills from the kernel CSPRNG; unencrypted ones carry zeros, as OpenSSH's do. A reserve that runs short makes a packet wait, so a slow top-up cannot be turned into a dropped connection. |
| Messages sent out of phase | What RFC 4253 §7.1 forbids during an exchange is refused on receipt and held on sending; a method message outside an exchange, a second `SSH_MSG_KEXINIT` in one, and a `SSH_MSG_NEWKEYS` with no keys installed for it each end the connection. |
| Terminal injection through peer text | Banner lines, `SSH_MSG_DEBUG`, and `SSH_MSG_DISCONNECT` text surface as the peer's raw bytes and are marked untrusted; whatever displays them sanitises first. A peer's identification is kept exactly as sent, because the exchange hash covers it. |

Every refusal is a typed error that closes the connection and, where the peer
can still be told, queues an `SSH_MSG_DISCONNECT` naming the reason. The text
is fixed; nothing the peer sent is reflected in it.

## Audit events

`lib/ssh::events` holds the `24_000..25_000` range and lands with `sshd`
(`plans/SSH.md` S5), the first component that emits them.
