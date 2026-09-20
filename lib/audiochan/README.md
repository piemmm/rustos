# tairix-audiochan

TAIRiX audio device-channel driver side: the `audiochan-v1` server every audio
driver *process* runs (`plans/SOUND.md` SND4). Stability tier:
**experimental** — it tracks the unfrozen `abi-v1` `audiochan-v1` contract.

The mixer service (`audiod`) runs in its own address space and owns the shared
PCM regions; an audio driver owns its device (MMIO, DMA, interrupt) and serves
a call endpoint. This crate is everything a driver process must do *around* an
opened `Audio` device, written once rather than copied per device, so a driver
is device bring-up plus one `serve` call.

It is separate from `lib/audio` for the reason `lib/netchan` is separate from
`lib/net`: a driver process must not link the mixer.

## The two halves

* `server` — `AudioChannelServer`, the pure, host-testable request handler:
  the per-endpoint unconfigured/configured/attached state machine, the
  geometry validation, and the period service. It performs no I/O, so the
  whole control plane is exercised on the host against a mock `Audio`.
* `serve` — the freestanding process loop (bare-metal targets only): it
  claims a reserved device-channel endpoint bound restricted-sender on
  `CAP_AUDIO_DEVICE`, emits the `audiochan` hardware-tree node the device
  manager hands to the mixer, and parks on a wait set over `{call endpoint,
  device interrupt}`.

## State is per endpoint, not per channel

A device presents several sinks and sources and each is driven independently,
so one channel carries several endpoints' state. `Configure` programs the
hardware and records the grant; `Attach` validates the offered ring against
*that grant* through `ConfigureGrant::geometry` — the single derivation both
sides size the region from, so they cannot disagree about how large it is. A
reconfiguration drops the attached region rather than leaving it the wrong
shape, and the serve loop unmaps it in the same step.

A grant whose period rounds up past its own ring ceiling admits no
power-of-two ring at all, so it is refused as a device fault at `Configure`
rather than at every later `Attach`.

## The interrupt path services, and that is deliberate

A period interrupt *means* "the device has consumed a period; refill it". The
region is already mapped in the driver, so making the mixer ask for the refill
with a blocking call would cost two extra process switches per period on the
one path in the system whose whole job is not to have jitter — and would put
the refill deadline behind the mixer's scheduling latency rather than the
driver's. The driver moves the period itself and then sends one notify
carrying the `(position, sampled_at)` pair the mixer's linear clock fit is
built from. The ring's atomic counters are what make that safe.

The device's event sources are masked whenever nothing is attached, so a
device left clocking cannot storm a driver with nowhere to put frames, and
they are released again on the mixer's next `Service`. An under- or over-run
is reported as the *delta* since the previous service, because the wire report
already carries the running total.

Nothing polls: between events the process parks on its wait set, and the
device's own period interrupt is the only timer in the stack.

## Fail closed

Every reply is a fully-encoded `audiochan-v1` frame carrying a typed `Errno`:
an endpoint index the device does not present, a transport call before attach,
a region that does not match the agreed geometry or is not aligned for the
ring counters, or any device fault. Never a panic, never a partially-applied
action — a refused attach leaves no state and no mapping, and a release the
hardware refused still forgets the channel, because the process is about to
unmap the region either way. Set-up refusals in `serve` return a reserved
`exit` code (the same numbers `lib/netchan` uses, so one supervisor table
reads both classes), so a driver that cannot serve ends with a diagnosable
reason rather than degrading into a busy re-poll.

An autoloaded driver is detached, so nothing reads its `stderr` and the exit
code alone would name only the stage that gave up. Every such refusal is
therefore recorded through `fail`, which writes the reason — and the typed
refusal behind it, where there was one — to the system log before returning
the code. It lives here, beside the codes, so two audio drivers cannot
describe the same failure differently.
