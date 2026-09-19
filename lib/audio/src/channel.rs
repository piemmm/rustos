//! Channel mapping: the explicit matrix that carries a source layout onto a
//! sink layout.
//!
//! # Explicit, or refused
//!
//! Every system that "handles" a layout mismatch by copying channel zero into
//! every output has silently turned a surround mix into mud. Here each source
//! position has a stated fan-out — itself where the sink carries it, a
//! documented pair of substitutes where it does not — and a pair of layouts
//! with no defined relationship is [`Errno::NotSupported`], not a guess.
//!
//! # The coefficients
//!
//! The surround-to-stereo coefficients are ITU-R BS.775's: centre and the
//! surrounds enter both fronts at one over root two, which preserves their
//! power across the fold. BS.775 excludes the low-frequency channel from the
//! downmix, so a sink without one drops it **by rule** rather than by
//! accident, and that is the only position with no substitute.
//!
//! The side pair is the conventional extension of BS.775 to 7.1, which the
//! recommendation itself predates: sides prefer the rears' slot and otherwise
//! fold like them.
//!
//! Upmixing synthesises nothing. A sink channel no source position reaches is
//! silent, because inventing a surround channel from a stereo recording is a
//! creative decision and not a conversion.
//!
//! # Equal layouts are the identity
//!
//! Every position maps to itself at unity, so a stream whose layout the device
//! already carries passes through multiplied by exactly one — which is what
//! the stack's bit-exactness claim needs from this stage.

use tairix_abi::driver::audio::{ChannelMap, ChannelPosition, MAX_CHANNELS};
use tairix_abi::Errno;

/// One over the square root of two: the power-preserving coefficient a
/// position folded into two others enters each of them with.
const FOLD: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// The coefficient a rear or side position enters a monophonic sink with:
/// folded into the front pair, then folded again into the single channel.
const FOLD_TO_MONO: f32 = FOLD * 0.5;

/// A source position's destination in a sink that carries it directly, or the
/// substitutes it folds into when the sink does not.
///
/// Ordered by preference: the first entry whose every position the sink
/// carries is the one used, so a 7.1 source folds to 5.1 before it folds to
/// stereo. An exhausted list is a refusal.
struct FanOut {
    /// The position this rule is for.
    source: ChannelPosition,
    /// Candidate destinations, best first. Each is a slice of (position,
    /// coefficient) pairs; an empty slice means the position is dropped by
    /// rule rather than for want of anywhere to put it.
    options: &'static [&'static [(ChannelPosition, f32)]],
}

/// Every source position's fan-out, which is what makes this mapping a stated
/// policy rather than whatever the loop happened to do.
const FAN_OUT: &[FanOut] = &[
    FanOut {
        source: ChannelPosition::Mono,
        options: &[
            &[(ChannelPosition::Mono, 1.0)],
            &[
                (ChannelPosition::FrontLeft, 1.0),
                (ChannelPosition::FrontRight, 1.0),
            ],
            &[(ChannelPosition::FrontCentre, 1.0)],
        ],
    },
    FanOut {
        source: ChannelPosition::FrontLeft,
        options: &[
            &[(ChannelPosition::FrontLeft, 1.0)],
            &[(ChannelPosition::Mono, 0.5)],
            &[(ChannelPosition::FrontCentre, FOLD)],
        ],
    },
    FanOut {
        source: ChannelPosition::FrontRight,
        options: &[
            &[(ChannelPosition::FrontRight, 1.0)],
            &[(ChannelPosition::Mono, 0.5)],
            &[(ChannelPosition::FrontCentre, FOLD)],
        ],
    },
    FanOut {
        source: ChannelPosition::FrontCentre,
        options: &[
            &[(ChannelPosition::FrontCentre, 1.0)],
            &[
                (ChannelPosition::FrontLeft, FOLD),
                (ChannelPosition::FrontRight, FOLD),
            ],
            &[(ChannelPosition::Mono, FOLD)],
        ],
    },
    FanOut {
        source: ChannelPosition::LowFrequency,
        // BS.775 excludes the low-frequency channel from a downmix: it carries
        // no programme material a two-channel listener is missing, and folding
        // it in would only eat headroom.
        options: &[&[(ChannelPosition::LowFrequency, 1.0)], &[]],
    },
    FanOut {
        source: ChannelPosition::RearLeft,
        options: &[
            &[(ChannelPosition::RearLeft, 1.0)],
            &[(ChannelPosition::SideLeft, 1.0)],
            &[(ChannelPosition::FrontLeft, FOLD)],
            &[(ChannelPosition::Mono, FOLD_TO_MONO)],
        ],
    },
    FanOut {
        source: ChannelPosition::RearRight,
        options: &[
            &[(ChannelPosition::RearRight, 1.0)],
            &[(ChannelPosition::SideRight, 1.0)],
            &[(ChannelPosition::FrontRight, FOLD)],
            &[(ChannelPosition::Mono, FOLD_TO_MONO)],
        ],
    },
    FanOut {
        source: ChannelPosition::SideLeft,
        options: &[
            &[(ChannelPosition::SideLeft, 1.0)],
            &[(ChannelPosition::RearLeft, 1.0)],
            &[(ChannelPosition::FrontLeft, FOLD)],
            &[(ChannelPosition::Mono, FOLD_TO_MONO)],
        ],
    },
    FanOut {
        source: ChannelPosition::SideRight,
        options: &[
            &[(ChannelPosition::SideRight, 1.0)],
            &[(ChannelPosition::RearRight, 1.0)],
            &[(ChannelPosition::FrontRight, FOLD)],
            &[(ChannelPosition::Mono, FOLD_TO_MONO)],
        ],
    },
];

/// The resolved coefficients carrying one source layout onto one sink layout.
///
/// Derived once when a stream opens and applied per period, so the policy
/// above is read at configuration time and never on the sample path.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ChannelMatrix {
    /// `weights[destination][source]`, zero where a source does not reach a
    /// destination.
    weights: [[f32; MAX_CHANNELS]; MAX_CHANNELS],
    sources: usize,
    destinations: usize,
    /// Whether the matrix is the identity, which lets the sample path skip the
    /// multiply-accumulate entirely and stay bit-exact.
    identity: bool,
}

impl ChannelMatrix {
    /// Derive the matrix carrying `source` onto `sink`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotSupported`] when a source position has nowhere defined to
    /// go in `sink`. Copying it somewhere arbitrary would be a silent
    /// corruption of the material, so the pair is refused and the caller
    /// re-opens against a layout it can serve.
    pub fn derive(source: &ChannelMap, sink: &ChannelMap) -> Result<Self, Errno> {
        let mut matrix = Self {
            weights: [[0.0; MAX_CHANNELS]; MAX_CHANNELS],
            sources: source.positions().len(),
            destinations: sink.positions().len(),
            identity: source == sink,
        };
        for (index, position) in source.positions().iter().enumerate() {
            let rule = FAN_OUT
                .iter()
                .find(|rule| rule.source == *position)
                .ok_or(Errno::NotSupported)?;
            let chosen = rule
                .options
                .iter()
                .find(|option| {
                    option
                        .iter()
                        .all(|(destination, _)| sink.positions().contains(destination))
                })
                .ok_or(Errno::NotSupported)?;
            for (destination, weight) in *chosen {
                let slot = sink
                    .positions()
                    .iter()
                    .position(|candidate| candidate == destination)
                    .ok_or(Errno::NotSupported)?;
                matrix.weights[slot][index] += *weight;
            }
        }
        Ok(matrix)
    }

    /// Interleaved channels the matrix reads.
    #[must_use]
    pub const fn sources(&self) -> usize {
        self.sources
    }

    /// Interleaved channels the matrix writes.
    #[must_use]
    pub const fn destinations(&self) -> usize {
        self.destinations
    }

    /// Whether the mapping is the identity, so the sample path may copy.
    #[must_use]
    pub const fn is_identity(&self) -> bool {
        self.identity
    }

    /// The coefficient source channel `source` enters destination channel
    /// `destination` with, or zero outside the matrix.
    #[must_use]
    pub fn weight(&self, destination: usize, source: usize) -> f32 {
        self.weights
            .get(destination)
            .and_then(|row| row.get(source))
            .copied()
            .unwrap_or(0.0)
    }

    /// Map `frames` interleaved frames from `src` into `dst`, scaling every
    /// contribution by `gain`.
    ///
    /// `dst` is overwritten rather than accumulated into: the mixer's first
    /// contributor assigns and later ones add, which is what keeps a lone
    /// stream's samples — negative zero included — exactly its own.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when either side is shorter than `frames`
    /// needs.
    pub fn map(&self, frames: usize, gain: f32, src: &[f32], dst: &mut [f32]) -> Result<(), Errno> {
        if src.len() < frames * self.sources || dst.len() < frames * self.destinations {
            return Err(Errno::BufferTooSmall);
        }
        // An exact comparison on purpose: the copy below is bit-exact and a
        // gain within an epsilon of unity is not, so a tolerance here would
        // silently substitute one for the other.
        #[allow(clippy::float_cmp, reason = "exactness is the condition being tested")]
        let untouched = self.identity && gain == 1.0;
        if untouched {
            dst[..frames * self.destinations].copy_from_slice(&src[..frames * self.sources]);
            return Ok(());
        }
        for frame in 0..frames {
            let inputs = &src[frame * self.sources..frame * self.sources + self.sources];
            let outputs =
                &mut dst[frame * self.destinations..frame * self.destinations + self.destinations];
            for (destination, slot) in outputs.iter_mut().enumerate() {
                let mut sum = 0.0;
                for (source, sample) in inputs.iter().enumerate() {
                    sum += *sample * self.weights[destination][source];
                }
                *slot = sum * gain;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "channel_tests.rs"]
mod tests;
