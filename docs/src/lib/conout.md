# `tairix-conout`

The console-output engine every architecture port shares: how kernel console
output is queued, ordered, shed under pressure, accounted for, and handed to a
device.

A console has two halves. The *device* half — which register carries a byte,
how readiness is reported, how an interrupt is armed — is genuinely
architecture-specific and lives in each `kernel/arch/<target>/` port. The
*policy* half is identical on every port, so it lives here once: a port that
gets console output wrong gets it wrong in one place, and a fix reaches every
port.

## Why a queue at all

A character transmitter carries a few thousand bytes a second while the CPU
issues millions of instructions in the same time. A producer that pushed its
line to the device byte by byte would stall whatever it was doing — a driver
bring-up, an input pump, a syscall — for the duration of the transmission. So a
producer copies its bytes into memory and returns, and the device is fed
afterwards by an interrupt or, on a port with no completion interrupt, by the
producer's own bounded drain.

## Frames, not bytes

Output is queued as **frames** — one whole record, or one accepted run of a
program's output — each with a header carrying its length and class, and a
footer carrying its length again so the queue can walk backwards from the tail
to find the newest frame.

That framing is what makes the queue's guarantees expressible. A byte FIFO can
only drop *bytes*, which truncates a line mid-way and lets the next line's
bytes fill the gap — corruption that looks like a hardware fault. A frame queue
refuses or evicts a **whole** line, so what reaches the wire is always a
sequence of complete lines.

## Ordering and whole-line integrity

A frame is admitted while holding `OutQueue`'s gate, an
`IrqSafeSpinLock` that masks the *producing CPU's own* interrupts for the hold.
That closes both ways a line could be torn:

- Another CPU logging concurrently waits its turn, rather than racing the
  drain.
- An interrupt handler that logs on the same CPU cannot re-enter mid-line,
  because it is masked out for the (short, device-free) hold.

The hold is a memory copy and a non-blocking device push — never a wait on the
transmitter — so masking interrupts for it costs a bounded, tiny window.

## Pressure: shedding by severity, never silently

When the queue is full:

| Class of output | What happens |
|---|---|
| A record | May **evict the newest, less severe** record to make room, so a critical diagnostic is not lost behind debug chatter. Eviction stops at an equally-severe record. |
| A program's own output | Never evicted, and never dropped to make room for a record. A full queue yields a short write the caller retries. |
| The frame being transmitted | Never evicted, so the device's in-flight line is never cut. |

Every refused or evicted frame is counted — records **and** bytes — and the
count is emitted as a `Warn` record (`CONSOLE_OUTPUT_DROPPED`, 18001) at the
point in the stream where the gap actually is. A report that cannot itself be
queued charges its count back, so no gap is ever forgotten. This is what makes
the sink honest as both a log and an audit sink: loss is visible, never silent.

## Bounded waiting, and the wedged transmitter

`tx_wait` is the one readiness-wait policy. A transmitter is polled up to a
budget; expiry declares it **wedged** and the byte is dropped rather than
hanging the kernel — an unbounded readiness spin would stall the machine on its
first log line, which is exactly what a flow-blocked or unwired line produces
on real hardware. The verdict is sticky: a wedged transmitter costs one poll per
byte, not a full budget per byte, and recovers the instant it accepts one.

## The bypass path

If the gate cannot be acquired within a bounded number of attempts, its holder
is presumed dead — a CPU that died inside the critical section — and the record
is written straight to the device. This is the **only** path that bypasses the
queue. It exists because a console must keep working while the system is dying,
and its bytes can no longer interleave with a queued line: the queue they would
have raced can never drain again.

## What a port supplies

`ConsoleTx`: `send_ready` (what the device will take right now, without
waiting), `send_bounded` (with the bounded wait), `send_bypass` (one byte on the
dead-holder path), `set_completion_interrupt`, and `uptime_ms` for the record
stamp. A port with no completion interrupt sets `COMPLETION_INTERRUPT = false`
and the engine drains write-through instead.

## Sizing

`DEFAULT_CAPACITY_BYTES` follows the build profile: a development build streams
a verbose boot log and is given room for a whole bursty driver bring-up; a
shippable build logs sparingly and would rather have the memory. The storage is
a fixed reservation rather than a capacity grown from discovered memory, because
the console must carry the first boot record long before a page allocator
exists — and what does not fit is counted and reported, so the bound is visible.
