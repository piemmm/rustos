# Audio streams (`audio-v1`)

`tairix_abi::audio` is the one surface a program plays or records sound
through. There is no second client API, no exclusive mode, and no raw device
node: a program enumerates the sinks and sources it may see, opens a stream, is
told the latency it was granted, shares a PCM ring, and writes frames at exact
positions.

The staged design is `plans/SOUND.md`; the device-side half is
[audio drivers](../drivers/audio.md).

## What the protocol deliberately does not have

**No period or buffer size.** Those are the device's ring geometry. Every
system that leaks them into its client API makes every program re-derive
latency from numbers it should never have seen. A client states a latency
*target* and is told the latency it was *granted* — in frames **and** in
`Duration64`, so it never needs a sample rate to reason about time.

**No mix format.** A request the device cannot meet is answered with what it
*can* meet rather than silently resampled, so a single stream at unity gain
whose rate and format the device accepts reaches the hardware unaltered.
Bit-exactness is a property of the one path, not a mode beside it — which is
exactly why no bypass is needed.

## Operations

| Request | Reply |
|---|---|
| `Enumerate { direction, index }` | `AudioDeviceDescriptor`, or `NotFound` past the end |
| `Open(OpenParams)` | `StreamGrant` — what was *actually* granted |
| `Attach { stream_id, region_grant }` | status |
| `Start` / `Stop` at a frame, `Drain`, `Flush` | status |
| `Clock { stream_id }` | `ClockReport` |
| `Gain`, `Mute` | status |
| `State { stream_id }` | `StreamReport` |
| `Close { stream_id }` | status |

`Open` answers before the region exists, because the ring's geometry is part of
the answer: the client sizes its `shm_create` from `StreamGrant::ring_frames`
and then `Attach`es it. A `stream_id` is a service-issued token checked against
the kernel-attested caller, so a guessed id cannot reach another principal's
stream, and zero is reserved — it is what a truncated frame carries.

A client that simply wants "the speakers" opens on device zero. Enumerated
device identities start at one, so zero names no real device and is free to
mean *this machine's default in the direction asked for*. That spares the
common case a round trip it would otherwise spend enumerating, and spares it
the race of the default changing between that enumeration and the open. A
machine with no default adopted refuses the open rather than substituting a
device the caller never asked for.

## Role, not a configuration file

`StreamRole` is the one input a program gives the router beyond its format:
`Media`, `Communication`, `Notification`, or `Accessibility`. Routing, ducking,
and what survives a seat switch are decided from the role by one policy
function, never from a list of process names. A `Notification` from a session
that does not hold the seat is dropped rather than queued — one that arrives
ten minutes late is noise, and the role is what says so.

## Positions and the exported clock

Every position is a monotone `Frames` count, so `Start` at a frame, gapless
playback, and A/V sync are exact arithmetic rather than a guess.

`ClockReport` hands back the device's own `(position, sampled_at)` pair **and
its measured rate**: a device whose crystal says 48 000 and whose reality says
47 998.6 reports the second. That is what makes cross-device drift a number
rather than a mystery, and it is why `StreamGrant::clock_domain` is worth
carrying — two streams sharing a domain share a clock exactly, and moving
between domains is a re-open with the position carried across, never a hidden
resampler.

## States

`StreamState` is `Idle`, `Running`, `Paused`, `Draining`, `SeatInactive`, and
`DeviceLost`. The last two are the ones worth naming here:

* **`SeatInactive`** — the session does not hold the sink's seat lease, so the
  stream is paused at a frame boundary and *told so*. A departing user's music
  does not play into the arriving user's room, and it does not silently vanish
  either: on switch-back it resumes from the exact frame.
* **`DeviceLost`** — the device went away. The position is intact, the reason
  is stated, and other devices are untouched.

`StreamReport` carries both glitch tallies, because neither implies the other:
the frame count says how much audio was missed, the event count says how often
the user heard it.

## The notify port

A client parks on its stream's mailbox and never polls its ring.
`AudioNotify` is `SpaceAvailable`, `StateChanged` (with the exact frame it
changed at), and `Xrun` (with the frames lost). The position never lies, so a
client resynchronises exactly rather than drifting.

The port's id is **derived by the service** from the caller's kernel-attested
pid (`notify_endpoint_for`) and handed back in the grant; the client binds it
but does not choose it. A client that could name its own notify port could name
somebody else's mailbox instead and use the audio service as a proxy to spam
it, and no check the service could make would tell the two apart. The id is
deliberately not a reserved rendezvous, so binding it needs no privileged bind,
and a grant naming one is refused at decode.

## Authority

Playback needs **no capability**: the authorisation is that the caller's
session holds the seat lease on the sink, checked at open against the
kernel-attested caller. That is a more precise check than a capability, and a
capability every program would hold is not a boundary.

Opening a *source* additionally demands the capture capability, and every live
capture stream is machine state the session draws a recording indicator from —
a recording program cannot suppress it. Monitoring a sink's own mix is
authorised by holding that seat's lease, so a session may monitor its own
output and nothing may monitor another principal's; no third capability is
needed because the lease already expresses exactly the right boundary.

## Fail closed

Every decode is total: an unknown magic, version, operation or role, a dirty
reserved field, an out-of-range rate or latency, a channel map with a repeated
position, a zero stream id, or a grant naming a reserved rendezvous refuses
with one typed `Errno`. `lib/abi/tests/fuzz_audio.rs` drives every decoder on
this page with mutated and pure-noise frames.
