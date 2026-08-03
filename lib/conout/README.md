# `tairix-conout` — console output engine

The one definition of *how kernel console output is queued and delivered*,
shared by every architecture port.

A console has two halves. One is the device: which register carries a byte,
how readiness is reported, how an interrupt is armed. That half is genuinely
architecture-specific and lives in each `kernel/arch/<target>/` port. The
other half is the policy — what a line is, what order lines go out in, what
happens when more output is produced than the device can carry, and how the
operator finds out. That half is identical on every port, so it lives here
once.

## What it guarantees

- **A line is delivered whole, or not at all.** Output is queued as *frames*,
  not loose bytes, and a frame is admitted under a lock that masks the
  producing CPU's own interrupts. Two CPUs logging at the same instant
  therefore cannot interleave their bytes, and neither can an interrupt
  handler that logs while the CPU it fired on was mid-line.
- **Nothing is lost silently.** A refused or evicted frame is counted, in
  records and bytes, and the count is reported as a warning record on the wire
  at the point in the stream where the gap actually is. If the report itself
  cannot be queued, the count is charged back rather than forgotten.
- **Under pressure the least important line goes first.** A record may evict a
  *newer, less severe* record to make room, so a critical diagnostic is not
  lost behind a flood of debug chatter. A program's own output is never
  evicted, and neither is a frame the device has already started
  transmitting.
- **Waiting on a device is bounded.** A transmitter that never reports itself
  ready is declared wedged and its bytes dropped (and counted). An unbounded
  readiness spin would hang the kernel on its first log line.
- **A dying CPU cannot silence the console.** If the queue's lock cannot be
  acquired within a bounded number of attempts its holder is presumed dead, and
  the record is written straight to the device instead. This is the *only*
  path that bypasses the queue, and it exists so a diagnostic about the failure
  still reaches the wire.

## What a port supplies

One trait, `ConsoleTx`: send what the device will take right now (without
waiting), send with a bounded wait, send one byte on the bypass path, and arm
or disarm the completion interrupt. A port that has no completion interrupt
says so with one associated constant and the engine drains write-through
instead.

## Stability

`experimental` — the surface is consumed by the three bare-metal architecture
ports and may still change shape as the fourth port's console lands.
