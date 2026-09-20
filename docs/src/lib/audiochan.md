# `tairix-audiochan`

`lib/audiochan` is the **driver side of the `audiochan-v1` audio
device-channel contract** (`plans/SOUND.md` SND4): everything an audio driver
process must do around an opened device to serve the mixer, written once so
every audio driver shares one control plane.

## Why it exists

The mixer service and an audio driver run as separate processes. The mixer
owns the shared PCM regions and is the channel's *client*; the driver owns the
device (MMIO/DMA/IRQ) and is its *server*. The wire codecs for that contract
live in `lib/abi::driver::audio_channel`, but the *server behaviour* is not a
wire type — it is per-endpoint configuration and attach state, geometry
validation, and a wait-set loop.

It is separate from `lib/audio` for the reason `lib/netchan` is separate from
`lib/net`: **a driver process must not link the mixer.** The engine that
decides what samples come out is one crate and the device-channel server is
another, so a sound card's driver carries no mixing code at all.

## Two layers

`AudioChannelServer<A: Audio>` is the pure, host-testable per-request handler.
It performs no I/O: the caller receives the request, maps the granted regions,
and sends the reply this server produces, so the whole control plane is
exercised on the host against a mock device.

`serve` is the freestanding process loop, compiled only for the bare-metal
targets a driver binary is built for. It claims a reserved device-channel
endpoint bound **restricted-sender on `CAP_AUDIO_DEVICE`**, publishes the
`tairix,audiochan` hardware-tree node the device manager hands to the mixer,
and parks on a wait set over `{call endpoint, device interrupt}`.

## State is per endpoint

A device presents several sinks and sources and each is driven independently,
so one channel carries several endpoints' state rather than one channel's.

- An endpoint starts **unconfigured**: `Facts` and `EndpointFacts` answer and
  every transport call refuses with `NotConnected`.
- `Configure` programs the hardware and records the grant — the device's own
  answer about what it will actually run at.
- `Attach` validates the offered ring against *that recorded grant*, through
  `ConfigureGrant::geometry`: the single derivation both sides size the region
  from, so they cannot disagree about how large it is. A refused attach leaves
  no state and no mapping.
- A reconfiguration drops the attached region rather than leaving it the wrong
  shape, and the serve loop unmaps it in the same step.
- `Detach` releases the endpoint's device-side stream. A release the hardware
  refused still forgets the channel state, because the process is about to
  unmap the region either way.

A grant whose period rounds up past its own ring ceiling admits no
power-of-two ring at all, so `ConfigureGrant::validate` refuses it as a device
fault at `Configure` rather than at every later `Attach`. The same validation
runs on the mixer's side of the wire, so a grant that reached the mixer is one
the mixer can size a region from.

## The interrupt path services, and that is deliberate

A period interrupt *means* "the device has consumed a period; refill it". The
region is already mapped in the driver, so making the mixer ask for the refill
with a blocking call would cost two extra process switches per period on the
one path in the system whose whole job is not to have jitter — and would put
the refill deadline behind the mixer's scheduling latency rather than the
driver's.

So the driver moves the period itself and then sends one notify carrying the
`(position, sampled_at)` pair the mixer's linear clock fit is built from. The
ring's atomic counters are what make that safe, and its release/acquire edge
carries a `loom` model in `lib/abi`.

An under- or over-run is reported as the **delta** since the previous service,
because the wire report already carries the running total and the notify is
about what just happened.

## Nothing spins

Between events the process parks on its wait set. The device's event sources
are masked whenever nothing is attached — so a device left clocking cannot
storm a driver with nowhere to put frames — and released again on the mixer's
next `Service`. The device's own period interrupt is the only timer in the
stack.

## Fail closed

Every reply is a fully-encoded `audiochan-v1` frame carrying a typed `Errno`:
an endpoint index the device does not present, a transport call before attach,
a region that does not match the agreed geometry or is not aligned for the ring
counters, or any device fault. Never a panic, never a partially-applied
action. Set-up refusals in `serve` return a reserved `exit` code — the same
numbers `lib/netchan` uses, so one supervisor table reads both classes — so a
driver that cannot serve ends with a diagnosable reason rather than degrading
into a busy re-poll.

## Stability

**experimental** — it tracks the unfrozen `abi-v1` `audiochan-v1` contract.
