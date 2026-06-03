# `rustos-abi`

Defines the frozen `abi-v1` interface between the kernel and user space.

## Scope

* `Errno` — stable `#[repr(i32)]` error codes.
* `CapabilityId` — dense `u16` identifier for kernel capabilities, bounded
  by `CAPABILITY_ID_MAX`.
* `SyscallNumber` — `u16` syscall identifier; kernel dispatch tables index
  this directly.
* `IpcMessageHeader` — 32-byte little-endian header carried in front of
  every IPC message.
* `PointerInput` — 20-byte little-endian desktop pointer event (absolute
  move, or a resolved primary/secondary/middle press/release) the window
  manager and taskbar route. Distinct from the device-level
  `driver::input::InputEvent` (see [Input events](../abi/input.md)).
* `KeyInput` — 20-byte little-endian desktop keyboard event (a press or
  release carrying a `KeyValue` — a produced `Char` or a `NamedKeyCode` —
  and the held `Modifiers`) the window manager delivers to the focused
  window (see [Input events](../abi/input.md)).
* `ManifestHeader` — fixed-size prefix of the signed `rxe` manifest section,
  including the SHA-256 syscall-table fingerprint, the Ed25519 public key,
  and the signature.

## Wire format

All multi-byte fields are little-endian regardless of host endianness. The
total wire size of every struct is exposed via a `WIRE_LEN` constant and
asserted equal to `size_of::<T>()` in the unit tests; any padding regression
fails the build.

## Compatibility

The numeric values of every public constant are part of the contract.
Existing values are never re-numbered or removed. New entries take the next
free integer; a structural change ships as `abi-v2` and the old types move
into a compatibility submodule rather than mutate in place
(`AGENTS.md` §9).

## Stage boundary

Stage 1 ships the *numbering* of syscalls in `lib/abi/src/syscall.rs`
(singular). The cross-checked file `lib/abi/src/syscalls.rs` and its
generated counterpart `kernel/syscall/src/table.rs` are introduced together
in Stage 2 so `cargo xtask abi-check` always sees both halves.
