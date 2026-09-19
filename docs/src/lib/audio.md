# `tairix-audio`

`lib/audio` is the **audio engine**: everything that decides *what samples
come out* (`plans/SOUND.md`). It is `no_std`, carries no `unsafe`, and
performs no I/O, opens no window and issues no syscall, so every decision the
stack makes about a sample is testable on a host with no machine attached.

## Why it exists, and why it is not the service

The split between `lib/sound` and `lib/audio` is the tree's own precedent
applied: `lib/image` decodes picture files and `lib/raster` draws pixels, so
`lib/sound` decodes sound files and `lib/audio` moves samples. Neither knows
the other. A decoder answers PCM and has no idea a device exists; the engine
mixes PCM and has no idea a file format exists. The one place they meet is a
player.

`lib/audiochan` — the device-channel serve loop — is a separate crate for the
reason `lib/netchan` is separate from `lib/net`: a driver process must not
link the mixer. The driver serves; the mixer is the one client.

## The property everything else is shaped around

> A source of twenty-four bits or fewer, at unity gain, at a rate and channel
> map the device accepts, with no other stream live, reaches the device
> **bit-exact**.

This is why TAIRiX has no exclusive or bypass mode. Windows needs one because
its shared path resamples and re-quantises unconditionally; remove the cause
and the mode is unnecessary. The claim is a property of the one path, so each
stage is built to be the identity in that case rather than merely close:

| Stage | Why it is exact |
|---|---|
| `convert` | Every scale factor is a power of two, so a twenty-four-bit integer divides into `f32` and multiplies back with no rounding at all. |
| `volume` | `millibel_to_linear(0)` is a special case returning exactly `1.0`, not an exponential that lands near it. |
| `channel` | Equal layouts derive the identity matrix, whose `map` at unity gain is a `copy_from_slice`. |
| `resample` | An equal rate pair bypasses the filter entirely. A windowed sinc at unity is *very nearly* the identity, and "very nearly" would destroy the property. |
| `mix` | The **first** contributor is assigned into the accumulator rather than added to a zeroed one, because `0.0 + -0.0` is `+0.0`. |

`lib/audio/tests/bit_exact.rs` drives it over five encodings × five rates ×
five channel layouts × ten block lengths, through every stage a real playback
stream passes.

**The one documented exception.** A thirty-two-bit *integer* source carries
twenty-four bits of mantissa through the `f32` pivot, so its low eight bits do
not survive. That is stated rather than rescued: no consumer format produces
meaningful thirty-two-bit integer audio, and an `f64` accumulator would cost
every other path to save a case nobody can hear. The test asserts the bounded
error rather than skipping the format.

## Conversion, and where dither belongs

The pivot is `f32` normalised to full scale. `convert` is total over all
thirty-six (source, destination) pairs, implemented as decode-then-encode with
a straight copy where the two encodings are equal — writing thirty-six direct
converters beside the mixer's own pivot would be the same arithmetic spelled
twice.

Quantising to fewer bits than the material carried correlates the truncation
error with the signal, which is audible as distortion rather than hiss.
Triangular-probability dither of one destination step decorrelates it, and is
applied **only** where the destination is genuinely narrower — the mixer makes
that test from the contributors' own resolutions, bumped to the pivot's
twenty-four bits whenever more than one stream contributed or a stream came
through the resampler, because a sum carries content below every contributor's
own step.

## Untrusted samples

A mixer reads frames a client wrote into a shared region, so those bytes cross
a trust boundary. A `NaN` reaching a shared accumulator would silence every
other stream on the sink, and a float far outside full scale would swamp them.
Both are bounded at the boundary: a non-finite sample decodes as silence, an
out-of-scale one at the encoding's own full scale, and a twenty-four-in-
thirty-two word outside the twenty-four bits it claims to carry at that
encoding's full scale rather than at up to two hundred and fifty-six times it.
One tenant's numbers never bound another's.

## The one resampler

A polyphase Kaiser-windowed-sinc interpolator. There is exactly one in TAIRiX:
drivers never resample and clients never need to, so a second implementation
anywhere is a review blocker.

**The ratio never drifts.** The output position is an integer input frame plus
an integer remainder over the reduced denominator — 48 kHz into 44.1 kHz is
160/147 and the step is exactly that — so no floating-point accumulator is
advanced per sample and an hour of playback ends on precisely the frame the
arithmetic says.

**One mechanism, two cases.** The bank holds one row per denominator step
where the denominator fits, which covers every pair in the standard rate
family (147, 160, 320 and the telephony multiples): the phase lookup is then
an index and the interpolation weight between rows is exactly zero. A larger
denominator — a rate pair with no common factor, or the drifting ratio a
linked clock domain corrects against — shares the same stepping and reaches
its phase by interpolating between the two nearest rows, which is the
fractional-delay case. The code has one path; the exact case simply never
engages the interpolation.

**The figures are measured, not asserted.**

| | |
|---|---|
| Passband edge | `0.43` of the lower rate — 19.0 kHz at 44.1 kHz |
| Stopband edge | Nyquist of the lower rate, so nothing folds back into the band |
| Stopband rejection | **≥ 100 dB**, measured |
| Passband ripple | **< 0.1 dB**, measured |
| Taps per output | 96 when interpolating; scaled by the decimation factor when not, capped at 512 |

`resample_tests.rs` reassembles the prototype from the built bank's own
coefficients, evaluates its frequency response, and holds it to those
constants. A change to the tap count or the window that quietly degraded the
filter fails the suite rather than leaving this page lying.

A decimating filter must reach the stopband by the *output's* Nyquist, so its
transition is narrower in input-rate terms by exactly the decimation factor
and it needs proportionally more taps. The cap covers decimation to a fifth,
which is past every pair in the standard family; a steeper ratio widens the
transition rather than unbounding the work one output sample costs. The bank
depends on the ratio alone, so one bank per (source rate, sink rate) serves
every stream on a device rather than each carrying its own.

## Channel mapping

Every source position has a stated fan-out — itself where the sink carries it,
a documented pair of substitutes where it does not — and a pair of layouts
with no defined relationship is refused rather than guessed at. Every system
that "handles" a mismatch by copying channel zero into every output has
silently turned a surround mix into mud.

The surround-to-stereo coefficients are ITU-R BS.775's: centre and each
surround enter both fronts at one over root two. BS.775 excludes the
low-frequency channel from the downmix, so a sink without one drops it **by
rule** rather than by accident, and that is the only position with no
substitute. The side pair is the conventional extension of BS.775 to 7.1,
which the recommendation predates: sides prefer the rears' slot and otherwise
fold like them.

Upmixing synthesises nothing. A sink channel no source position reaches is
silent, because inventing a surround channel from a stereo recording is a
creative decision and not a conversion.

## The clock model

Every period a driver reports the pair (device frame position, the `Time64` it
was sampled at). A least-squares fit over a bounded window of those pairs
gives the device's *real* rate — 47 998.6 Hz for a card whose crystal says
48 000 — and the map between its frames and the wall clock. That map is what
makes gapless playback and A/V synchronisation arithmetic instead of a guess,
and it turns drift between two devices into a reported number rather than a
mystery nobody can debug.

The pairs come from a driver process and are treated as such: a position or a
timestamp that went backwards, or a pair implying a rate no converter runs at,
is refused and left out of the fit. Below eight observations, or where the fit
lands outside a plausible band around the nominal rate, the model **says it
does not know** and the nominal rate stands — a crystal is accurate to parts
per million, so a fit that says otherwise is measuring something else.

## The routing policy

A pure function from (role, who holds the sink, which sink was asked for) to
what happens to a stream, so it is host-tested over the whole cross-product
rather than discovered by experiment. A program says what its sound is *for*
and nothing else; no process names appear anywhere.

- A sink no seat has claimed plays for anybody, which is the headless case.
- A session holding the sink's lease has its streams mixed.
- A session that does not gets **paused at a frame boundary and told so**,
  holding its position, so a switch back resumes on the frame it stopped on. A
  departing user's music does not play into the arriving user's room, and it
  does not silently vanish either.
- A `Notification` from an inactive session is **dropped, not queued**: one
  that arrives ten minutes late is noise, and the role is what says so.
- A named sink that does not exist, and a machine with no configured default,
  are refusals rather than a sink picked arbitrarily.

Media steps aside for speech — a `Communication` or `Accessibility` stream on
the same sink attenuates it by 20 dB — and nothing else ducks.

## The volume model

Four independent gains — per stream, per application, per sink, and the
router's ducking — in hundredths of a decibel, which is the unit a codec's own
amplifier capability word converts into without a scale factor. Decibels
compose by addition, so they sum, and the sum is split once between the
hardware control the device has and the one multiply the mixer applies.

The hardware setting is rounded to the step *above* the target, so the
remainder software applies is always attenuation: rounding the other way would
leave software making the difference up with gain, on a path with no headroom
to spare. Where the target lands on the device's own grid — every whole
decibel on most codecs — the software remainder is zero and the multiply is
exactly one, which is how a volume setting and the bit-exact path coexist.

## The stream client

The half a program links: open, attach the shared region, start and stop at
exact positions, drain, flush, read the clock, set gain and mute, read the
state, close, and park on the notify mailbox. It holds no capability, opens no
endpoint and issues no syscall — the IPC round trip and the park are the
caller's, supplied through the `AudioTransport` seam, which keeps the client
host-testable against a mock service and keeps this crate free of I/O.

A client names the **frame** its samples belong at. Where that is ahead of
what the ring already carries, the distance is closed with the format's own
silence first, so the position the service reads never lies about where the
samples that follow belong. Where it is behind, the write is refused: those
frames are published and may already have been played, and quietly dropping
the request would leave the caller believing they were not.

## Performance and memory

Every working buffer is allocated when a sink is configured and reused, so the
per-period path allocates nothing, locks nothing, and does work bounded by
(live streams × period frames). Glitchless output is then a property of the
arrangement rather than of the machine's mood.

A mixer's buffers are tens of kibibytes per sink. A resampler holds only its
per-channel history — a few kibibytes — because the coefficients live in the
shared bank. A bank is bounded by its phase and tap caps together, so no rate
pair a client asks for can demand an unbounded coefficient table.

The sample-conversion kernel is resolved once through `lib/cpuops`, under that
framework's capability gate and mandatory self-verify against the portable
reference. There are no accelerated candidates today — the portable kernel is
a per-sample shift and multiply the compiler already vectorises — and the seam
exists so one can be added under the gate rather than beside it.
