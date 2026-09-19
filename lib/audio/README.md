# tairix-audio

Stability tier: **experimental**

Everything that decides *what samples come out*. `lib/sound` decodes sound
files and this crate moves samples, exactly as `lib/image` decodes pictures
and `lib/raster` draws them — neither knows the other.

`no_std`, `forbid(unsafe_code)`, no I/O, no window, no syscall. Every decision
the stack makes about a sample is therefore testable on a host with no machine
attached. The mixer service (`audiod`) and the device-channel serve loop
(`lib/audiochan`) are separate crates, for the reason `lib/netchan` is separate
from `lib/net`: a driver process must not link the mixer.

## What it provides

| Module | What it decides |
|---|---|
| `convert` | The saturating map between every encoding and the `f32` pivot, and where dither belongs. |
| `channel` | Which source channel reaches which sink channel, and at what coefficient. |
| `resample` | The one rate conversion in the system. |
| `mix` | How live streams sum into one period of device frames. |
| `clock` | What a device's rate actually is, and the map between its frames and the wall clock. |
| `route` | Which sink a stream lands on, and what a seat switch does to it. |
| `volume` | Four gains resolved into one multiply and one number to show. |
| `stream` | The client half of `audio-v1` — the part a program links. |

## The property the crate exists to keep

**A source of twenty-four bits or fewer, at unity gain, at a rate and channel
map the device accepts, with no other stream live, reaches the device
bit-exact.**

Every stage is built so it is the identity in that case rather than merely
close to it:

- the pivot's scale factors are powers of two, so a twenty-four-bit integer
  divides into `f32` and multiplies back with no rounding;
- `millibel_to_linear(0)` is exactly `1.0`, not approximately;
- an identical channel map derives the identity matrix, whose `map` is a copy;
- an equal rate pair bypasses the filter entirely, because a windowed sinc at
  unity is very nearly — and not exactly — the identity;
- the mixer **assigns** its first contributor rather than adding to a zeroed
  accumulator, because `0.0 + -0.0` is `+0.0` and a float source that wrote a
  negative zero is entitled to read it back.

`tests/bit_exact.rs` drives this over the cross-product of five encodings,
five rates, five channel layouts and ten block lengths, through every stage a
real stream passes.

A thirty-two-bit **integer** source carries twenty-four bits of mantissa
through the pivot. That is documented rather than rescued: no consumer format
produces meaningful thirty-two-bit integer audio, and an `f64` accumulator
would cost every other path to save a case nobody can hear.

## The resampler's measured figures

One polyphase Kaiser-windowed-sinc interpolator, and a second implementation
anywhere in the tree is a review blocker.

| | |
|---|---|
| Passband edge | `0.43` of the lower rate (19.0 kHz at 44.1 kHz) |
| Stopband edge | Nyquist of the lower rate |
| Stopband rejection | ≥ 100 dB, **measured** |
| Passband ripple | < 0.1 dB, **measured** |
| Taps per output | 96, scaled by the decimation factor, capped at 512 |

The figures are not asserted in prose: `resample_tests.rs` reassembles the
prototype from the built bank's own coefficients, evaluates its frequency
response, and holds it to the constants above. A change that quietly degraded
the filter fails the suite rather than leaving the documentation lying.

The ratio is an exact rational carried as an integer frame plus an integer
remainder, so an hour of playback ends on precisely the frame the arithmetic
says. Where the reduced denominator fits the bank — which is every pair in the
standard rate family — the phase lookup lands on a row exactly and the
interpolation weight is zero; a larger denominator reaches its phase by
interpolating between rows, which is the fractional-delay case, over the same
stepping.

## Untrusted samples

A mixer reads frames a client wrote into a shared region. A `NaN` propagated
into a shared accumulator would silence every other stream on the sink, and a
float far outside full scale would swamp them, so an out-of-spec sample is
bounded at the boundary: non-finite lands as silence, out-of-scale at the
encoding's own full scale. One tenant's numbers never bound another's.
`tests/fuzz_engine.rs` drives every stage over hostile samples, gains,
layouts and ratios and asserts what reaches a device is always deliverable
audio.

## What it deliberately does not have

- **No second mixer, resampler, or client API.** That is the mess the design
  exists to avoid.
- **No exclusive or bypass path.** The bit-exactness property removes its
  reason for existing.
- **No synthesis on upmix.** A sink channel no source position reaches is
  silent; inventing a surround channel from a stereo recording is a creative
  decision, not a conversion.
- **No guessing on a layout pair with no defined relationship.** It is
  refused, because copying a channel somewhere arbitrary is a silent
  corruption of the material.

See `plans/SOUND.md` and `docs/src/lib/audio.md`.
