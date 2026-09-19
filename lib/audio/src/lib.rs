//! TAIRiX audio engine: everything that decides *what samples come out*.
//!
//! `lib/sound` decodes sound files and this crate moves samples, exactly as
//! `lib/image` decodes pictures and `lib/raster` draws them. Neither knows the
//! other: a decoder answers PCM with no idea a device exists, and the engine
//! mixes PCM with no idea a file format does. The one place they meet is a
//! player.
//!
//! Nothing here performs I/O, opens a window, or issues a syscall, so every
//! decision the stack makes about a sample is testable on a host with no
//! machine attached. The service that drives it (`audiod`) and the device
//! channel it drives (`lib/audiochan`) are separate, for the reason
//! `lib/netchan` is separate from `lib/net`: a driver process must not link
//! the mixer.
//!
//! # What the parts are
//!
//! | Module | What it decides |
//! |---|---|
//! | [`convert`] | The saturating map between every encoding and the `f32` pivot, and where dither belongs. |
//! | [`channel`] | Which source channel reaches which sink channel, and at what coefficient. |
//! | [`resample`] | The one rate conversion in the system. |
//! | [`mix`] | How live streams sum into one period of device frames. |
//! | [`clock`] | What a device's rate actually is, and the map between its frames and the wall clock. |
//! | [`route`] | Which sink a stream lands on, and what a seat switch does to it. |
//! | [`volume`] | Four gains resolved into one multiply and one number to show. |
//! | [`stream`] | The client half of `audio-v1` — the part a program links. |
//!
//! # The property the whole crate exists to keep
//!
//! **A source of twenty-four bits or fewer, at unity gain, at a rate and
//! channel map the device accepts, with no other stream live, reaches the
//! device bit-exact.** Every stage is built so that it is the identity in
//! that case rather than merely close to it: the pivot's scale factors are
//! powers of two, unity gain is exactly `1.0`, an identical channel map is a
//! copy, an equal rate bypasses the filter, and the mixer's first contributor
//! is assigned rather than added to a zeroed accumulator. The claim is
//! property-tested across formats, rates, channel counts and block
//! boundaries, not asserted here.
//!
//! That is why there is no exclusive mode anywhere in TAIRiX: the thing such
//! a mode exists to escape does not happen on this path.
//!
//! # Untrusted samples
//!
//! A mixer reads frames a client wrote. A `NaN` propagated into a shared
//! accumulator would silence every other stream on the sink, so a non-finite
//! or out-of-scale sample is bounded before it is summed. One tenant's
//! numbers never bound another's.
//!
//! The design and its rationale are `plans/SOUND.md`; the page is
//! `docs/src/lib/audio.md`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod channel;
pub mod clock;
pub mod convert;
pub mod mix;
pub mod resample;
pub mod route;
pub mod stream;
pub mod volume;

pub use channel::ChannelMatrix;
pub use clock::ClockModel;
pub use convert::{Dither, DitherSource};
pub use mix::{Mixer, SinkFormat, StreamMix};
pub use resample::{FilterBank, Ratio, Resampler};
pub use route::{Routing, SinkState, StreamRequest};
pub use stream::{AudioTransport, StreamClient, Written};
pub use volume::{ResolvedVolume, VolumeRequest};
