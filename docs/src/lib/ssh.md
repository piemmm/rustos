# `tairix-ssh`

The SSH protocol engine (`plans/SSH.md`). It performs no I/O and reaches
neither a clock nor a random-number generator, so the code the tests and fuzz
harnesses exercise is the code a connection's worker runs — a
`SPAWN_FLAG_SANDBOX` process whose whole world is two pipes. What the protocol
needs from a clock or an RNG arrives as input; what it produces is output the
host carries.

This page covers the transport layer: the wire codec, the identification
exchange, the binary packet protocol, and the state machine above it. Key
exchange, keys, authentication, and the connection protocol are the later
increments of `plans/SSH.md`. What the layer defends against, and how, is
[the SSH security page](../security/ssh.md).

## Modules

| Module | Holds |
|---|---|
| `wire` | The RFC 4251 §5 data types. `Reader` decodes borrowing from its input; `Writer` appends and latches its first failure. `Mpint` and `NameList` exist only in canonical form, so a value that decodes re-encodes to exactly the bytes it came from. |
| `msg` | The transport's message numbers, the RFC 4250 block a number belongs to, what RFC 4253 §7.1 lets be sent mid-exchange, and `DisconnectReason`. |
| `ident` | `Ident`, an identification string kept byte for byte (it is `V_C`/`V_S` of the exchange hash), and `next_line`, the bounded line splitter a peer's opening bytes go through. |
| `algorithm` | `Cipher` and `Mac`: every algorithm `plans/SSH.md` §4 admits, each name and length defined once. `Keys`: one direction's derived material, zeroed on drop. |
| `packet` | `Sealer` and `Opener`: one direction each of the binary packet protocol under every framing, with the sequence space and the RFC 4344 counters. |
| `transport` | `Transport`: both directions, the phase rules, strict key exchange, the held queues, and the padding reserve. |

## Driving a transport

```rust,ignore
let mut transport = Transport::new(Role::Server, Ident::new("TAIRiX_1.0", None)?)?;
// Bytes from the socket; a short count is back-pressure.
let taken = transport.receive(&bytes)?;
while let Some(item) = transport.next_received()? {
    match item {
        Received::Message { sequence, payload } => { /* the layer above */ }
        Received::NewKeys => { /* the peer switched keys */ }
        Received::Banner(_) | Received::Debug { .. } => { /* untrusted text */ }
        Received::Unimplemented { .. } | Received::Disconnected { .. } => {}
    }
}
transport.send(msg, &[header, data])?;       // gathered, sealed in place
socket_write(transport.pending_output());      // then consume_output(n)
if transport.padding_wanted() > 0 { /* ask the host's CSPRNG */ }
```

Everything `next_received` returns borrows the transport's buffer until the
next call, so a payload is read where it was decrypted. `send` takes the body
as a list of slices and copies each once, straight into the frame it is sealed
in, so channel data needs no staging buffer of its own.

**The layer above acts on each key-exchange message before pulling the
next.** After the peer's first `SSH_MSG_KEXINIT` it settles strictness with
`set_strict` — the transport opens nothing further until it has, and refuses
the attempt rather than guessing — and after deriving keys it hands them in
with `install_keys` before the peer's `SSH_MSG_NEWKEYS` is pulled. It sends
`SSH_MSG_NEWKEYS` with `send_newkeys`, and closes with `disconnect`; `send`
refuses both numbers, so the key switch and the close each have one path.

## What waits, and why

A message the transport cannot seal now is held, never dropped and never
reordered:

- **The exchange gate.** Between this end's `SSH_MSG_KEXINIT` and its
  `SSH_MSG_NEWKEYS`, only the messages RFC 4253 §7.1 permits go out. The rest
  wait and leave, in order, under the new keys.
- **Padding.** An encrypted packet's padding comes from the reserve the host
  fills (`supply_padding`) when `padding_wanted` asks. A message the reserve
  cannot pad waits for the top-up; a slow host delays a packet and never ends
  a connection.
- **The exchange's own messages** queue apart and go first, and ordinary
  traffic may not spend a floor of the reserve kept back for them, so a rekey
  never stalls behind the traffic it is rekeying. A `SSH_MSG_NEWKEYS` that
  waits switches the keys when it is sealed, not when it was requested.

`is_holding` and `outbound_backlog` tell the layer above to stop producing
bulk data; `send` refuses with `Backlogged` at `MAX_BACKLOG`, a containment
bound that always admits one packet of the largest size into an empty backlog,
so the refusal is transient by construction.

Rekeying by volume is counted here: `rekey_due` turns true once either
direction's keys have carried 2³¹ packets or their cipher's byte bound
(2³² blocks for AES, a gigabyte for `chacha20-poly1305`, the bounds OpenSSH
applies), tightened by `set_rekey_limit`. Rekeying by time is the host's
timer calling `rekey_interval_elapsed`. Sending `SSH_MSG_KEXINIT` clears it.

## Bounds and memory

Every size here is a fixed validation bound on untrusted input, not a
capacity: `MAX_PACKET_LEN` (256 KiB, OpenSSH's own), `MAX_IDENT_LINE` (255,
RFC 4253's), `MAX_BANNER_LINE` and `MAX_BANNER_LINES` (OpenSSH's). Nothing a
peer sends moves them, and a length past one is refused as soon as it is
readable, before anything behind it is buffered.

The buffers behind those bounds are `lib/collections`'s `ByteQueue`s, which
commit storage as bytes arrive and never past the bound, so an idle
connection costs a few kilobytes and a hostile one at most its bound. An
allocation refused along the way closes the connection with `OutOfMemory`
rather than aborting the process. The queues wipe any storage they give back,
and keys are wiped when a direction rekeys or the engine is dropped.

## Tests

- Host unit tests beside each module, including every RFC 4251 §5 example.
- `interop_tests`: byte-exact packets OpenSSH 10.2p1 produced and accepted,
  captured under every cipher and MAC with strict key exchange. One test
  replays a whole `chacha20-poly1305` exchange through a `Transport` at
  several chunkings and requires its output to equal, byte for byte, what
  that `sshd` accepted.
- Three fuzz harnesses in `cargo xtask fuzz`: `fuzz_ssh_wire`,
  `fuzz_ssh_ident`, and `fuzz_ssh_packet`, the last driving two transports
  through random traffic, padding starvation, and rekeys in any interleaving.

## Stability

`experimental`. No `unsafe`; no panic on any input.
