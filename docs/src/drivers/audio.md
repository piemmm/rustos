# Audio drivers

An audio device is one that presents **sinks** and **sources**: PCM endpoints
the machine clocks samples out of or into. `HwDeviceClass::Audio` names the
class, drivers live under `drivers/audio/<leaf>/`, and every one of them runs
in user space, bound by discovery-match, holding only the register window, DMA
constraint and interrupt line its matched node requested. None is in the
bootstrap floor: nothing about reaching the driver store needs sound.

The staged design is `plans/SOUND.md`; this page is the driver-class view. The
client half a program plays through is
[`audio-v1`](../abi/audio.md).

## One path, and where a driver sits in it

```
program ── audio-v1 ──▶ audiod ── audiochan-v1 ──▶ driver ──▶ hardware
              (PCM ring)          (PCM ring)
```

A driver never speaks to a program, and a program never speaks to a driver.
The mixer service is the single client of every audio device, and it is the
sole holder of the audio-device capability, so the kernel refuses at dispatch
every other caller of a driver's endpoint. A driver therefore never
re-checks: authority was settled before its code ran.

## The PCM vocabulary

`tairix_abi::driver::audio` is the vocabulary every layer shares, defined once
because four of them must agree on it exactly — the decoder that produces
samples, the engine that mixes them, the device channel that carries them, and
the driver that clocks them out.

| Type | What it fixes |
|---|---|
| `SampleFormat` | the six encodings the stack converts between, and each one's silence byte |
| `ChannelMap` | the interleave order, positions unique, `Mono` only alone |
| `Rate` | a sample rate inside the range a real converter runs at |
| `RateSupport` | what a device can be clocked at: a discrete list, or a continuous range |
| `Frames` | a monotone position from the start of a stream |
| `GainRange` | a hardware gain control in hundredths of a decibel |
| `AudioEndpointFacts` | everything the mixer needs to configure one sink or source |

Two details are load-bearing rather than decorative. Unsigned eight-bit PCM's
silence is `0x80`, not zero, so `SampleFormat::silence_byte` exists and a gap
filled without it would click. And `RateSupport` models both a crystal-driven
codec's handful of discrete rates *and* a USB Audio Class 2 clock source's
continuous range, because modelling only the first would force a continuous
device to publish an invented list.

There is no period or buffer *setting* in the facts — only the bounds the
hardware imposes. The depth is derived from those bounds and the client's
latency target, so no `const` period size exists anywhere in the stack.

## The PCM ring

`tairix_abi::driver::audio_ring` is one structure serving both hops, because
it is the same job twice: one producer appends interleaved frames, one
consumer takes them, and the two run concurrently in different address spaces.

The header carries two free-running `u64` **frame** positions — total produced
and total consumed — and they never wrap: at 192 kHz a `u64` runs for about
three million years. Occupancy is their plain difference and the slot index is
a mask of the low bits, so the class of wrap-around bugs that byte-indexed
rings spend their lives fixing does not arise. It also means a position on the
wire and a position in the ring are the same number, which is what makes
"start at frame N" and "we lost frames N..M" exact arithmetic.

The ordering discipline is the usual one and is stated in the crate: the
producer writes a frame's bytes then **releases** its position; the consumer
**acquires** that position before reading those bytes and releases its own only
once it has finished with them. The two positions sit in separate cache lines,
because in one line every publish would invalidate the peer's read of the
other. `lib/abi/tests/audio_ring_spsc.rs` drives both sides concurrently and
asserts every frame crosses exactly once, in order, intact.

Both positions live in memory the *peer* can write, so every operation
snapshots them once, refuses a backwards or over-full pair as
`Errno::OutOfRange`, and works from that snapshot. Even the publish arithmetic
is checked: a peer that parks the producer position at the top of the counter
cannot make a write overflow it.

## `audiochan-v1` — the device channel

`tairix_abi::driver::audio_channel` is the control plane, shaped from
`netchan-v1` with one deliberate difference: **configuration comes before
attachment**.

| Operation | What it does |
|---|---|
| `Facts` | what the device is, and how many endpoints it presents |
| `EndpointFacts` | what one sink or source can do |
| `Configure` | program rate, format, channel layout and period — answered with what the device could actually meet |
| `Attach` | hand over the granted sample region and name the notify port |
| `Start` / `Stop` / `Drain` | transport, at exact frame positions |
| `Service` | the doorbell: move one period, and report the clock pair |
| `Gain` | hardware gain and mute |
| `Detach` | release the channel |

A device answers `Configure` with a `ConfigureGrant` — the rate it *will* run
at rather than a refusal — and `ConfigureGrant::geometry` is the one place both
sides derive the shared region's shape from, so they cannot disagree about how
large it is.

Notifications run the other way: `PeriodElapsed` carries the `(position,
sampled_at)` pair the mixer's per-device clock fit is built from, `Xrun` says
exactly which frames were lost, and `JackChanged` reports a connector. A frame
a notification's kind does not define must be zero, so a second meaning cannot
be smuggled into the fixed frame.

### Discovery and the endpoint block

A driver claims the first free id in the reserved
`AUDIO_CHANNEL_ENDPOINT_BASE` block (spelled `"ACHAN"`), so two audio drivers
never collide without a central allocator. Binding a reserved id requires
`CAP_IPC_BIND_PRIVILEGED`, so an unprivileged squatter cannot impersonate a
driver; the driver additionally binds it restricted-sender on the audio-device
capability, so only the mixer can reach it.

The driver then publishes a hardware-tree node carrying
`AUDIOCHAN_NODE_COMPATIBLE` (`tairix,audiochan`), which the device manager
recognises as a bound audio device's channel and hands to the mixer. The key
is defined beside the endpoint block so the key emitted and the key looked for
cannot drift.

### The driver copies, on purpose

Once per period a driver copies between the shared ring and its own DMA
buffer. A zero-copy arrangement would mean publishing the driver's DMA window
to another process; the driver owning that window absolutely is worth more
than the copy costs, which at 48 kHz stereo 32-bit and a five-millisecond
period is under 400 KiB/s.

### Nothing spins

Between doorbells a driver parks on its device interrupt. When a period
elapses it wakes the mixer, and the mixer — parked on that port in its wait
set — issues the next `Service`. The device's own period interrupt is the only
timer in the stack.

## Fail closed

Every decode on this surface is total and validates whole. An unknown magic,
version or operation byte, a dirty reserved field, an endpoint index past the
device's own bound, a channel map with a repeated position, a ring depth the
index arithmetic could not serve, or a notification carrying a field its kind
does not define refuses with one typed `Errno` rather than guessing.
`lib/abi/tests/fuzz_audio.rs` drives every decoder on this page with mutated
and pure-noise frames, and drives the ring over positions a hostile peer could
have written.

## The class trait

`tairix_abi::driver::audio::Audio` is what every `drivers/audio/*` engine
implements and what `lib/audiochan`'s serve loop is written once over, so the
whole control plane exists in one place rather than per device. It is
deliberately the device's own vocabulary and nothing above it: report the
device's facts and each endpoint's, `configure` an endpoint and answer what
the hardware will actually run at, `start`/`stop`/`drain` it, move one period
between the caller's ring view and the device with `service`, `set_gain`,
`release`, and — for the driver process's interrupt path — `take_interrupt`
and `set_event_interrupts`.

There is no mixing, no conversion and no routing here, because those belong to
the one engine in `lib/audio`; a driver that did any of them would be a second
one.

`AudioInterrupt` is what an endpoint's interrupt had to say, as three bitmaps
over endpoint index — period elapsed, under/over-run, jack changed. A bitmap
rather than a list because a device with several streams running signals them
together and the serve loop must not allocate on the interrupt path.

## The first driver: `drivers/audio/virtio_snd`

Virtio sound (virtio 1.2 §5.14, device type 25) over the bus-agnostic
split-virtqueue transport, on either bus: the single-aperture virtio-MMIO
device a `-M virt` machine presents and the scattered virtio-PCI device a PC
presents. One signed bundle binds both. It is the cheapest complete driver and
the one that gives an end-to-end QEMU vertical on every Tier-1 architecture,
so the whole path is proved against it before a second driver exists.

What it reports is what the device said. Bring-up reads the device's own
configuration and enumerates each jack, stream and channel map with an
information request; there is no table keyed on what the device claims to be.
Where the device publishes nothing the driver says so rather than inventing an
answer: no jacks means every endpoint reports `JackState::Unknown`, and no
channel maps means the conventional layout for the reported channel count — or
a refusal, for a count with no conventional reading.

A rate or encoding the device cannot do is **substituted** with the nearest it
can, so the mixer adapts and owns the conversion the difference implies.

### The position never lies

A running playback device must be fed every period. When the mixer's ring is
short *and the device has nothing left in flight*, the driver submits silence
for exactly the frames it is missing and adds them to the stream's loss tally.
Padding a period the device has not yet asked for would manufacture a glitch
out of frames that were merely going to arrive in time, so it does not; a
drain's tail goes as a short transfer rather than a padded one, because
nothing was lost. A capture period the mixer's ring could not hold is over-run,
and it is counted too.

The reported position is what the device has *clocked out*, not what it was
handed: the transfer status' `latency_bytes` is subtracted from the submitted
total.
