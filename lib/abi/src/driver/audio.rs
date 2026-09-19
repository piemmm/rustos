//! Audio device class (`drivers/audio/*`): the PCM vocabulary every layer of
//! the audio stack shares, and the facts a device reports about the sinks and
//! sources it presents (`plans/SOUND.md`).
//!
//! The vocabulary is defined once here because four independent layers must
//! agree on it exactly — the decoder that produces samples, the engine that
//! mixes them, the device channel that carries them, and the driver that
//! clocks them out. A sample format, a channel map or a frame position that
//! meant something slightly different in two of those would be a silent
//! corruption rather than a refusal.
//!
//! # Positions are frames, and frames are monotone
//!
//! Every position in the stack is a [`Frames`] count from the start of a
//! stream, never a byte offset and never a wrapping index. At 192 kHz a
//! `u64` frame counter runs for about three million years, so the whole
//! class of wrap-around arithmetic bugs is deleted rather than defended
//! against, and a client can name an exact frame to start, stop, or seek at.
//!
//! # Fail closed
//!
//! Every value here is validated at construction and re-validated on decode:
//! an undefined format or channel position, a duplicate position in a map, a
//! rate outside the range any real converter runs at, or a gain range whose
//! bounds are inverted is a typed [`Errno`], never a guess.

use crate::bounded_text::BoundedText;
use crate::le::{put_i32, put_u16, put_u32, read_i32, read_u16, read_u32};
use crate::time::Duration64;
use crate::Errno;

/// Channels one stream or device endpoint may carry.
///
/// A fixed validation bound, not a capacity: 7.1 surround is the widest
/// layout consumer hardware presents and the widest [`ChannelPosition`]
/// names, so a device or client claiming more is refused rather than
/// admitted into a mix whose channel matrix could not describe it.
pub const MAX_CHANNELS: usize = 8;

/// Discrete sample rates one device endpoint may advertise.
///
/// A fixed validation bound sized to the complete standard rate family —
/// the 8/16/32/64 kHz telephony-derived series, the 44.1 kHz CD series and
/// its multiples up to 768 kHz — which is exactly sixteen values. A device
/// claiming more is reporting nonsense, not a richer converter.
pub const MAX_DEVICE_RATES: usize = 16;

/// Bytes of a device or endpoint display name.
///
/// A fixed record bound: the name is one line of user-facing text ("Green
/// Line Out"), and a longer one would only be truncated by every surface
/// that draws it.
pub const AUDIO_NAME_MAX: usize = 32;

/// A device or endpoint display name — what a settings pane, `audioctl`, or
/// a player's device list shows the user.
pub type AudioName = BoundedText<0, AUDIO_NAME_MAX>;

/// Encoding of one PCM sample.
///
/// The closed set the engine converts between. Integer formats are
/// two's-complement little-endian and signed, with the sole exception of
/// [`Self::U8`], which is unsigned with mid-scale silence — the one
/// historical encoding still shipped by real hardware and real WAV files.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SampleFormat {
    /// Unsigned 8-bit; silence is `0x80`.
    U8 = 1,
    /// Signed 16-bit.
    S16 = 2,
    /// Signed 24-bit in three packed bytes.
    S24 = 3,
    /// Signed 24-bit sign-extended into a 32-bit container.
    S24In32 = 4,
    /// Signed 32-bit.
    S32 = 5,
    /// IEEE 754 binary32, nominally in `-1.0..=1.0`.
    F32 = 6,
}

impl SampleFormat {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an undefined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            1 => Ok(Self::U8),
            2 => Ok(Self::S16),
            3 => Ok(Self::S24),
            4 => Ok(Self::S24In32),
            5 => Ok(Self::S32),
            6 => Ok(Self::F32),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Bytes one sample occupies in memory.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S24In32 | Self::S32 | Self::F32 => 4,
        }
    }

    /// Bits of resolution the encoding actually carries, which is not its
    /// storage width for [`Self::S24In32`].
    #[must_use]
    pub const fn valid_bits(self) -> u32 {
        match self {
            Self::U8 => 8,
            Self::S16 => 16,
            Self::S24 | Self::S24In32 => 24,
            Self::S32 | Self::F32 => 32,
        }
    }

    /// Whether the encoding is floating point.
    #[must_use]
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32)
    }

    /// The byte value that fills a silent sample.
    ///
    /// Zero for every signed and floating encoding; mid-scale for
    /// [`Self::U8`], where a zero byte would be full negative deflection and
    /// a gap filled with it would be an audible click rather than silence.
    #[must_use]
    pub const fn silence_byte(self) -> u8 {
        match self {
            Self::U8 => 0x80,
            _ => 0x00,
        }
    }

    /// Bit position of this format within a [`SampleFormats`] set.
    const fn bit(self) -> u16 {
        1u16 << (self as u8 - 1)
    }
}

/// The set of [`SampleFormat`]s a device endpoint accepts.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct SampleFormats(u16);

impl SampleFormats {
    /// Every defined format's bit; any bit outside this is a corrupt set.
    const DEFINED: u16 = SampleFormat::U8.bit()
        | SampleFormat::S16.bit()
        | SampleFormat::S24.bit()
        | SampleFormat::S24In32.bit()
        | SampleFormat::S32.bit()
        | SampleFormat::F32.bit();

    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// `self` with `format` added.
    #[must_use]
    pub const fn with(self, format: SampleFormat) -> Self {
        Self(self.0 | format.bit())
    }

    /// Whether `format` is in the set.
    #[must_use]
    pub const fn contains(self, format: SampleFormat) -> bool {
        self.0 & format.bit() != 0
    }

    /// Whether the set names no format at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Raw on-wire bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Inverse of [`Self::bits`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when `bits` sets an undefined format bit — a
    /// corrupt set is refused rather than silently narrowed to the bits that
    /// happen to be recognised.
    pub const fn from_bits(bits: u16) -> Result<Self, Errno> {
        if bits & !Self::DEFINED != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }
}

/// Where one channel of a stream is meant to be heard.
///
/// The 7.1 vocabulary, which covers every layout consumer hardware presents.
/// A map's positions are what the channel matrix is derived from, so a
/// position the vocabulary does not name is refused rather than mapped to
/// something arbitrary.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ChannelPosition {
    /// The single channel of a monophonic stream.
    Mono = 1,
    /// Front left.
    FrontLeft = 2,
    /// Front right.
    FrontRight = 3,
    /// Front centre.
    FrontCentre = 4,
    /// Low-frequency effects.
    LowFrequency = 5,
    /// Rear (surround) left.
    RearLeft = 6,
    /// Rear (surround) right.
    RearRight = 7,
    /// Side left.
    SideLeft = 8,
    /// Side right.
    SideRight = 9,
}

impl ChannelPosition {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an undefined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            1 => Ok(Self::Mono),
            2 => Ok(Self::FrontLeft),
            3 => Ok(Self::FrontRight),
            4 => Ok(Self::FrontCentre),
            5 => Ok(Self::LowFrequency),
            6 => Ok(Self::RearLeft),
            7 => Ok(Self::RearRight),
            8 => Ok(Self::SideLeft),
            9 => Ok(Self::SideRight),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Wire length of a [`ChannelMap`]: the channel count then one position byte
/// per possible channel, unused slots zero.
pub const CHANNEL_MAP_WIRE_LEN: usize = 1 + MAX_CHANNELS;

/// The ordered positions of a stream's interleaved channels.
///
/// Order is the interleave order: `positions()[0]` is the first sample of
/// every frame. Positions are unique — two channels claiming the same
/// position would make the downmix matrix ambiguous — and [`Mono`] may only
/// appear alone, because a monophonic channel is by definition the whole
/// stream.
///
/// [`Mono`]: ChannelPosition::Mono
#[derive(Copy, Clone, Debug)]
pub struct ChannelMap {
    count: u8,
    positions: [ChannelPosition; MAX_CHANNELS],
}

/// The slots past the channel count are padding the constructors happen to
/// leave behind, not part of the map, so two equal layouts built by different
/// paths must compare equal.
impl PartialEq for ChannelMap {
    fn eq(&self, other: &Self) -> bool {
        self.positions() == other.positions()
    }
}

impl Eq for ChannelMap {}

impl ChannelMap {
    /// The single-channel map.
    pub const MONO: Self = Self::filled(1, ChannelPosition::Mono);

    /// The conventional two-channel map, front left then front right.
    pub const STEREO: Self = {
        let mut map = Self::filled(2, ChannelPosition::FrontLeft);
        map.positions[1] = ChannelPosition::FrontRight;
        map
    };

    /// A map of `count` slots whose every slot holds `position`, for the
    /// `const` constructors above to refine.
    const fn filled(count: u8, position: ChannelPosition) -> Self {
        Self {
            count,
            positions: [position; MAX_CHANNELS],
        }
    }

    /// Build a map from its interleave-ordered positions.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — empty, or more than [`MAX_CHANNELS`].
    /// * [`Errno::OutOfRange`] — a repeated position, or
    ///   [`ChannelPosition::Mono`] beside another channel.
    pub fn new(positions: &[ChannelPosition]) -> Result<Self, Errno> {
        if positions.is_empty() || positions.len() > MAX_CHANNELS {
            return Err(Errno::LengthOutOfRange);
        }
        for (index, position) in positions.iter().enumerate() {
            if positions[..index].contains(position) {
                return Err(Errno::OutOfRange);
            }
            if *position == ChannelPosition::Mono && positions.len() != 1 {
                return Err(Errno::OutOfRange);
            }
        }
        let mut map = Self::filled(
            u8::try_from(positions.len()).map_err(|_| Errno::LengthOutOfRange)?,
            ChannelPosition::Mono,
        );
        map.positions[..positions.len()].copy_from_slice(positions);
        Ok(map)
    }

    /// Number of interleaved channels.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.count
    }

    /// The positions, in interleave order.
    #[must_use]
    pub fn positions(&self) -> &[ChannelPosition] {
        &self.positions[..self.count as usize]
    }

    /// Encode into the fixed-width wire image.
    #[must_use]
    pub fn to_wire(&self) -> [u8; CHANNEL_MAP_WIRE_LEN] {
        let mut out = [0u8; CHANNEL_MAP_WIRE_LEN];
        out[0] = self.count;
        for (slot, position) in self.positions().iter().enumerate() {
            out[1 + slot] = position.as_u8();
        }
        out
    }

    /// Decode from the fixed-width wire image, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than
    ///   [`CHANNEL_MAP_WIRE_LEN`].
    /// * [`Errno::BadMagic`] — a non-zero byte past the declared count.
    /// * [`Errno::LengthOutOfRange`] / [`Errno::OutOfRange`] — as for
    ///   [`Self::new`].
    pub fn from_wire(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < CHANNEL_MAP_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let count = usize::from(bytes[0]);
        if count == 0 || count > MAX_CHANNELS {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[1 + count..CHANNEL_MAP_WIRE_LEN]
            .iter()
            .any(|b| *b != 0)
        {
            return Err(Errno::BadMagic);
        }
        let mut positions = [ChannelPosition::Mono; MAX_CHANNELS];
        for (slot, position) in positions.iter_mut().take(count).enumerate() {
            *position = ChannelPosition::from_u8(bytes[1 + slot])?;
        }
        // Rebuilt through the constructor so a hostile frame cannot smuggle a
        // duplicate position past the uniqueness rule the matrix relies on.
        Self::new(&positions[..count])
    }
}

/// A validated sample rate in hertz.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Rate(u32);

impl Rate {
    /// Lowest rate any converter in the vocabulary runs at.
    ///
    /// A fixed validation bound: 4 kHz is below the 8 kHz telephony floor, so
    /// nothing real is excluded, while a rate near zero would make every
    /// frames-to-time conversion in the stack degenerate.
    pub const MIN_HZ: u32 = 4_000;

    /// Highest rate any converter in the vocabulary runs at.
    ///
    /// A fixed validation bound: 768 kHz is the top of the 44.1/48 kHz
    /// multiple series and the highest rate consumer hardware advertises.
    pub const MAX_HZ: u32 = 768_000;

    /// The 48 kHz rate the stack defaults a device to when nothing else
    /// constrains it.
    pub const HZ_48000: Self = Self(48_000);

    /// Build a rate.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] outside [`Self::MIN_HZ`]`..=`[`Self::MAX_HZ`].
    pub const fn new(hz: u32) -> Result<Self, Errno> {
        if hz < Self::MIN_HZ || hz > Self::MAX_HZ {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(hz))
    }

    /// The rate in hertz.
    #[must_use]
    pub const fn hz(self) -> u32 {
        self.0
    }
}

/// Wire length of a [`RateSupport`]: kind, entry count, a reserved pair, then
/// the fixed-width entry array.
pub const RATE_SUPPORT_WIRE_LEN: usize = 4 + MAX_DEVICE_RATES * 4;

/// Wire byte for [`RateSupport::Discrete`].
const RATE_KIND_DISCRETE: u8 = 1;
/// Wire byte for [`RateSupport::Continuous`].
const RATE_KIND_CONTINUOUS: u8 = 2;

/// The rates one device endpoint can be clocked at.
///
/// Both shapes exist because both exist in hardware: a crystal-driven codec
/// offers a handful of discrete rates, and a PLL-driven USB Audio Class 2
/// clock source offers a continuous range. Modelling only the first would
/// force a continuous device to publish an invented list.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RateSupport {
    /// Exactly the listed rates, ascending.
    Discrete(RateSet),
    /// Every rate between the two bounds inclusive.
    Continuous {
        /// Lowest rate the clock can be programmed to.
        min: Rate,
        /// Highest rate the clock can be programmed to.
        max: Rate,
    },
}

impl RateSupport {
    /// Whether `rate` can be clocked exactly.
    #[must_use]
    pub fn admits(&self, rate: Rate) -> bool {
        match self {
            Self::Discrete(set) => set.contains(rate),
            Self::Continuous { min, max } => rate >= *min && rate <= *max,
        }
    }

    /// The supported rate closest to `wanted`, which is `wanted` itself when
    /// it is admitted.
    ///
    /// The one definition of "what can this device meet instead", so the
    /// driver answering a configure request and the mixer choosing a device
    /// rate cannot disagree. Ties go to the higher rate: resampling upward
    /// loses no band.
    #[must_use]
    pub fn nearest(&self, wanted: Rate) -> Rate {
        match self {
            Self::Discrete(set) => set.nearest(wanted),
            Self::Continuous { min, max } => wanted.clamp(*min, *max),
        }
    }

    /// Encode into the fixed-width wire image.
    #[must_use]
    pub fn to_wire(&self) -> [u8; RATE_SUPPORT_WIRE_LEN] {
        let mut out = [0u8; RATE_SUPPORT_WIRE_LEN];
        match self {
            Self::Discrete(set) => {
                out[0] = RATE_KIND_DISCRETE;
                out[1] = set.count;
                for (slot, rate) in set.rates().iter().enumerate() {
                    put_u32(&mut out, 4 + slot * 4, rate.hz());
                }
            }
            Self::Continuous { min, max } => {
                out[0] = RATE_KIND_CONTINUOUS;
                out[1] = 2;
                put_u32(&mut out, 4, min.hz());
                put_u32(&mut out, 8, max.hz());
            }
        }
        out
    }

    /// Decode from the fixed-width wire image, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than [`RATE_SUPPORT_WIRE_LEN`].
    /// * [`Errno::BadMagic`] — a dirty reserved field or a non-zero entry
    ///   past the declared count.
    /// * [`Errno::OutOfRange`] — an unknown kind byte, an out-of-range rate,
    ///   a continuous range whose bounds are inverted, or a discrete list
    ///   that is empty, over-long, or not strictly ascending.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < RATE_SUPPORT_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u16(bytes, 2) != 0 {
            return Err(Errno::BadMagic);
        }
        let count = usize::from(bytes[1]);
        match bytes[0] {
            RATE_KIND_DISCRETE => Self::discrete_from_wire(bytes, count),
            RATE_KIND_CONTINUOUS => Self::continuous_from_wire(bytes, count),
            _ => Err(Errno::OutOfRange),
        }
    }

    fn discrete_from_wire(bytes: &[u8], count: usize) -> Result<Self, Errno> {
        if count == 0 || count > MAX_DEVICE_RATES {
            return Err(Errno::OutOfRange);
        }
        Self::require_clean_tail(bytes, count)?;
        let mut rates = [Rate::HZ_48000; MAX_DEVICE_RATES];
        for (slot, rate) in rates.iter_mut().take(count).enumerate() {
            *rate = Rate::new(read_u32(bytes, 4 + slot * 4))?;
        }
        Ok(Self::Discrete(RateSet::new(&rates[..count])?))
    }

    fn continuous_from_wire(bytes: &[u8], count: usize) -> Result<Self, Errno> {
        if count != 2 {
            return Err(Errno::OutOfRange);
        }
        Self::require_clean_tail(bytes, count)?;
        let min = Rate::new(read_u32(bytes, 4))?;
        let max = Rate::new(read_u32(bytes, 8))?;
        if min > max {
            return Err(Errno::OutOfRange);
        }
        Ok(Self::Continuous { min, max })
    }

    /// Refuse a frame whose entry array carries anything past its declared
    /// count: a hidden value there would be a field the decoder never saw.
    fn require_clean_tail(bytes: &[u8], count: usize) -> Result<(), Errno> {
        if bytes[4 + count * 4..RATE_SUPPORT_WIRE_LEN]
            .iter()
            .any(|b| *b != 0)
        {
            return Err(Errno::BadMagic);
        }
        Ok(())
    }
}

/// An ascending, duplicate-free list of discrete sample rates.
#[derive(Copy, Clone, Debug)]
pub struct RateSet {
    count: u8,
    rates: [Rate; MAX_DEVICE_RATES],
}

/// The slots past the rate count are padding, not part of the set, for the
/// same reason a channel map's are.
impl PartialEq for RateSet {
    fn eq(&self, other: &Self) -> bool {
        self.rates() == other.rates()
    }
}

impl Eq for RateSet {}

impl RateSet {
    /// Build a set from `rates`.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — empty, or more than
    ///   [`MAX_DEVICE_RATES`].
    /// * [`Errno::OutOfRange`] — not strictly ascending, which also rejects
    ///   duplicates.
    pub fn new(rates: &[Rate]) -> Result<Self, Errno> {
        if rates.is_empty() || rates.len() > MAX_DEVICE_RATES {
            return Err(Errno::LengthOutOfRange);
        }
        if rates.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(Errno::OutOfRange);
        }
        let mut set = Self {
            count: u8::try_from(rates.len()).map_err(|_| Errno::LengthOutOfRange)?,
            rates: [Rate::HZ_48000; MAX_DEVICE_RATES],
        };
        set.rates[..rates.len()].copy_from_slice(rates);
        Ok(set)
    }

    /// The rates, ascending.
    #[must_use]
    pub fn rates(&self) -> &[Rate] {
        &self.rates[..self.count as usize]
    }

    /// Whether `rate` is in the set.
    #[must_use]
    pub fn contains(&self, rate: Rate) -> bool {
        self.rates().contains(&rate)
    }

    /// The listed rate closest to `wanted`, ties going to the higher rate.
    #[must_use]
    pub fn nearest(&self, wanted: Rate) -> Rate {
        let mut best = self.rates[0];
        for rate in self.rates() {
            let distance = rate.hz().abs_diff(wanted.hz());
            if distance <= best.hz().abs_diff(wanted.hz()) {
                best = *rate;
            }
        }
        best
    }
}

/// A position in a stream, counted in frames from its first.
///
/// One frame is one sample per channel. Positions are monotone and never
/// wrap, which is what lets a client name an exact frame to start, stop or
/// seek at and lets a glitch be accounted to the frames it lost.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct Frames(u64);

impl Frames {
    /// The start of a stream.
    pub const ZERO: Self = Self(0);

    /// A position `frames` frames from the start.
    #[must_use]
    pub const fn new(frames: u64) -> Self {
        Self(frames)
    }

    /// The position as a frame count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The position `frames` later, or [`None`] on overflow.
    #[must_use]
    pub const fn checked_add(self, frames: u64) -> Option<Self> {
        match self.0.checked_add(frames) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// Frames from `earlier` to `self`, or [`None`] when `earlier` is the
    /// later position.
    ///
    /// Fallible rather than saturating: a backwards span is a corrupt or
    /// hostile pair of positions, and answering zero would hide it.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Option<u64> {
        self.0.checked_sub(earlier.0)
    }

    /// How long `self` frames take to play at `rate`.
    ///
    /// Exact: the whole seconds and the sub-second remainder are computed
    /// separately, so an hour-long position accumulates no rounding and the
    /// nanosecond product — a remainder below the rate, times a billion —
    /// cannot overflow. Both fields are therefore always in range and the
    /// fallback below is unreachable.
    #[must_use]
    pub fn duration(self, rate: Rate) -> Duration64 {
        let hz = u64::from(rate.hz());
        let nanos = (self.0 % hz) * 1_000_000_000 / hz;
        Duration64::new(
            i64::try_from(self.0 / hz).unwrap_or(i64::MAX),
            u32::try_from(nanos).unwrap_or(0),
        )
        .unwrap_or(Duration64::ZERO)
    }

    /// How many whole frames `span` covers at `rate`, or [`None`] for a
    /// negative span or one longer than a frame counter can hold.
    #[must_use]
    pub fn from_duration(span: Duration64, rate: Rate) -> Option<Self> {
        let secs = u64::try_from(span.secs()).ok()?;
        let hz = u128::from(rate.hz());
        let whole = u128::from(secs).checked_mul(hz)?;
        let partial = u128::from(span.subsec_nanos()) * hz / 1_000_000_000;
        u64::try_from(whole.checked_add(partial)?).ok().map(Self)
    }
}

/// Which way samples flow through a stream or device endpoint.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StreamDirection {
    /// Towards the hardware: a sink.
    Playback = 0,
    /// From the hardware: a source.
    Capture = 1,
}

impl StreamDirection {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an undefined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Playback),
            1 => Ok(Self::Capture),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Whether anything is plugged into an endpoint's connector.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum JackState {
    /// The endpoint has no detection, or none has been performed yet.
    Unknown = 0,
    /// A connector is occupied.
    Present = 1,
    /// A connector is empty.
    Absent = 2,
}

impl JackState {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an undefined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::Present),
            2 => Ok(Self::Absent),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Wire length of a [`GainRange`] slot: presence, a reserved triple, the two
/// bounds, and the step.
pub const GAIN_RANGE_WIRE_LEN: usize = 4 + 4 + 4 + 4;

/// The hardware gain control an endpoint carries, in hundredths of a decibel.
///
/// Millibel because that is the unit every codec's amplifier capability word
/// converts into without a further scale factor, and because a decibel is too
/// coarse for a volume curve.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GainRange {
    min: i32,
    max: i32,
    step: u32,
}

impl GainRange {
    /// Build a gain range.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the bounds are inverted or the step is
    /// zero — a control that cannot move is absent, not a range.
    pub const fn new(
        min_millibel: i32,
        max_millibel: i32,
        step_millibel: u32,
    ) -> Result<Self, Errno> {
        if min_millibel > max_millibel || step_millibel == 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            min: min_millibel,
            max: max_millibel,
            step: step_millibel,
        })
    }

    /// Quietest setting, in hundredths of a decibel.
    #[must_use]
    pub const fn min_millibel(&self) -> i32 {
        self.min
    }

    /// Loudest setting, in hundredths of a decibel.
    #[must_use]
    pub const fn max_millibel(&self) -> i32 {
        self.max
    }

    /// Smallest change the control can make, in hundredths of a decibel.
    #[must_use]
    pub const fn step_millibel(&self) -> u32 {
        self.step
    }

    /// Encode an optional range into its fixed-width wire slot.
    #[must_use]
    pub fn to_wire(range: Option<Self>) -> [u8; GAIN_RANGE_WIRE_LEN] {
        let mut out = [0u8; GAIN_RANGE_WIRE_LEN];
        if let Some(range) = range {
            out[0] = 1;
            put_i32(&mut out, 4, range.min);
            put_i32(&mut out, 8, range.max);
            put_u32(&mut out, 12, range.step);
        }
        out
    }

    /// Decode an optional range from its fixed-width wire slot, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than [`GAIN_RANGE_WIRE_LEN`].
    /// * [`Errno::BadMagic`] — a dirty reserved byte, or a populated body on
    ///   an absent control.
    /// * [`Errno::OutOfRange`] — an undefined presence byte, or bounds that
    ///   fail [`Self::new`].
    pub fn from_wire(bytes: &[u8]) -> Result<Option<Self>, Errno> {
        if bytes.len() < GAIN_RANGE_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[1..4].iter().any(|b| *b != 0) {
            return Err(Errno::BadMagic);
        }
        match bytes[0] {
            0 => {
                if bytes[4..GAIN_RANGE_WIRE_LEN].iter().any(|b| *b != 0) {
                    return Err(Errno::BadMagic);
                }
                Ok(None)
            }
            1 => Ok(Some(Self::new(
                read_i32(bytes, 4),
                read_i32(bytes, 8),
                read_u32(bytes, 12),
            )?)),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Bounds on the ring a stream is carried in, so no hand-picked depth exists
/// anywhere in the stack.
///
/// A ring's depth is derived from the device's own reported period bounds and
/// the latency a client asked for; these two are the fixed validation bounds
/// that derivation is clamped into. They bound pinned memory a hostile device
/// or client could otherwise demand, so they are containment bounds rather
/// than capacities and do not scale with the machine.
pub mod ring_bounds {
    /// Fewest frames a ring may hold: one period being filled and one being
    /// consumed is the minimum that avoids strict lock-step.
    pub const MIN_FRAMES: u32 = 2;

    /// Most frames a ring may hold — 65 536, which is 1.37 seconds at 48 kHz
    /// and at most two mebibytes even at eight channels of 32-bit samples.
    /// Far past any latency an audio path wants, and a ceiling on the pinned
    /// memory one stream can reserve.
    pub const MAX_FRAMES: u32 = 1 << 16;
}

/// Wire length of an [`AudioDeviceFacts`]: the endpoint count, a reserved
/// pair, and the device's name.
pub const AUDIO_DEVICE_FACTS_WIRE_LEN: usize = 4 + 1 + AUDIO_NAME_MAX;

/// What one audio device reports about itself before its endpoints are
/// enumerated.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceFacts {
    /// Sinks and sources the device presents, each addressed by an index
    /// below this count.
    pub endpoints: u16,
    /// What to call the device in a user interface.
    pub name: AudioName,
}

impl AudioDeviceFacts {
    /// Encode into the fixed-width wire image.
    #[must_use]
    pub fn to_wire(&self) -> [u8; AUDIO_DEVICE_FACTS_WIRE_LEN] {
        let mut out = [0u8; AUDIO_DEVICE_FACTS_WIRE_LEN];
        put_u16(&mut out, 0, self.endpoints);
        out[4] = self.name.len_byte();
        out[5..].copy_from_slice(self.name.raw_bytes());
        out
    }

    /// Decode from the fixed-width wire image, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than
    ///   [`AUDIO_DEVICE_FACTS_WIRE_LEN`].
    /// * [`Errno::BadMagic`] — a dirty reserved pair.
    /// * [`Errno::OutOfRange`] — an endpoint count past
    ///   [`MAX_DEVICE_ENDPOINTS`], or a name that fails validation.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < AUDIO_DEVICE_FACTS_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u16(bytes, 2) != 0 {
            return Err(Errno::BadMagic);
        }
        let endpoints = read_u16(bytes, 0);
        if endpoints > MAX_DEVICE_ENDPOINTS {
            return Err(Errno::OutOfRange);
        }
        let mut name = [0u8; AUDIO_NAME_MAX];
        name.copy_from_slice(&bytes[5..AUDIO_DEVICE_FACTS_WIRE_LEN]);
        Ok(Self {
            endpoints,
            name: AudioName::from_wire(bytes[4], &name)?,
        })
    }
}

/// Sinks and sources one device may present.
///
/// A fixed validation bound, not a capacity: a sound card presents a handful
/// of jacks, and a device claiming hundreds is either broken or hostile —
/// enumerating them all would be a round trip each.
pub const MAX_DEVICE_ENDPOINTS: u16 = 32;

/// Byte offsets of each [`AudioEndpointFacts`] field, so the encoder, the
/// decoder, and the tests that corrupt one byte read one layout.
mod endpoint {
    use super::{AUDIO_NAME_MAX, CHANNEL_MAP_WIRE_LEN, GAIN_RANGE_WIRE_LEN, RATE_SUPPORT_WIRE_LEN};

    pub const INDEX: usize = 0;
    pub const DIRECTION: usize = 2;
    pub const JACK: usize = 3;
    pub const FORMATS: usize = 4;
    pub const RESERVED0: usize = 6;
    pub const CHANNEL_MAP: usize = 8;
    pub const RESERVED1: usize = CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN;
    pub const RATES: usize = RESERVED1 + 3;
    pub const MIN_PERIOD: usize = RATES + RATE_SUPPORT_WIRE_LEN;
    pub const MAX_PERIOD: usize = MIN_PERIOD + 4;
    pub const MAX_RING: usize = MAX_PERIOD + 4;
    pub const GAIN: usize = MAX_RING + 4;
    pub const NAME_LEN: usize = GAIN + GAIN_RANGE_WIRE_LEN;
    pub const NAME: usize = NAME_LEN + 1;
    pub const LEN: usize = NAME + AUDIO_NAME_MAX;
}

/// Wire length of an [`AudioEndpointFacts`].
pub const AUDIO_ENDPOINT_FACTS_WIRE_LEN: usize = endpoint::LEN;

/// What one sink or source of an audio device can do.
///
/// Everything the mixer needs to decide a device configuration, and nothing
/// it does not: there is no period or buffer *setting* here, only the bounds
/// the hardware imposes, because the depth is derived from those bounds and
/// the client's latency target rather than chosen from a constant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioEndpointFacts {
    /// The endpoint's index within its device, below
    /// [`AudioDeviceFacts::endpoints`].
    pub index: u16,
    /// Which way samples flow.
    pub direction: StreamDirection,
    /// Whether the connector is occupied.
    pub jack: JackState,
    /// Sample encodings the hardware accepts without conversion.
    pub formats: SampleFormats,
    /// The endpoint's channel layout.
    pub channel_map: ChannelMap,
    /// Rates the endpoint can be clocked at.
    pub rates: RateSupport,
    /// Fewest frames the device will interrupt on.
    pub min_period_frames: u32,
    /// Most frames the device will interrupt on.
    pub max_period_frames: u32,
    /// Most frames the device can hold in flight.
    pub max_ring_frames: u32,
    /// The hardware gain control, where the endpoint has one.
    pub gain: Option<GainRange>,
    /// What to call the endpoint in a user interface.
    pub name: AudioName,
}

impl AudioEndpointFacts {
    /// Check the facts are internally consistent.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — an endpoint index past
    ///   [`MAX_DEVICE_ENDPOINTS`], an empty format set, period bounds that
    ///   are inverted, or a ring bound outside the ring bounds or too small
    ///   to hold one period.
    pub fn validate(&self) -> Result<(), Errno> {
        if self.index >= MAX_DEVICE_ENDPOINTS || self.formats.is_empty() {
            return Err(Errno::OutOfRange);
        }
        if self.min_period_frames == 0 || self.min_period_frames > self.max_period_frames {
            return Err(Errno::OutOfRange);
        }
        // A period larger than the whole buffer could never be serviced, so a
        // device claiming one is reporting nonsense rather than a deep ring.
        if self.max_ring_frames > ring_bounds::MAX_FRAMES
            || self.max_ring_frames < self.max_period_frames
            || self.max_ring_frames < ring_bounds::MIN_FRAMES
        {
            return Err(Errno::OutOfRange);
        }
        Ok(())
    }

    /// Encode into the fixed-width wire image.
    #[must_use]
    pub fn to_wire(&self) -> [u8; AUDIO_ENDPOINT_FACTS_WIRE_LEN] {
        let mut out = [0u8; AUDIO_ENDPOINT_FACTS_WIRE_LEN];
        put_u16(&mut out, endpoint::INDEX, self.index);
        out[endpoint::DIRECTION] = self.direction.as_u8();
        out[endpoint::JACK] = self.jack.as_u8();
        put_u16(&mut out, endpoint::FORMATS, self.formats.bits());
        out[endpoint::CHANNEL_MAP..endpoint::CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN]
            .copy_from_slice(&self.channel_map.to_wire());
        out[endpoint::RATES..endpoint::RATES + RATE_SUPPORT_WIRE_LEN]
            .copy_from_slice(&self.rates.to_wire());
        put_u32(&mut out, endpoint::MIN_PERIOD, self.min_period_frames);
        put_u32(&mut out, endpoint::MAX_PERIOD, self.max_period_frames);
        put_u32(&mut out, endpoint::MAX_RING, self.max_ring_frames);
        out[endpoint::GAIN..endpoint::GAIN + GAIN_RANGE_WIRE_LEN]
            .copy_from_slice(&GainRange::to_wire(self.gain));
        out[endpoint::NAME_LEN] = self.name.len_byte();
        out[endpoint::NAME..].copy_from_slice(self.name.raw_bytes());
        out
    }

    /// Decode from the fixed-width wire image, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than
    ///   [`AUDIO_ENDPOINT_FACTS_WIRE_LEN`].
    /// * [`Errno::BadMagic`] — a dirty reserved field.
    /// * Whatever the embedded values' own decoders refuse, and whatever
    ///   [`Self::validate`] refuses.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < AUDIO_ENDPOINT_FACTS_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u16(bytes, endpoint::RESERVED0) != 0
            || bytes[endpoint::RESERVED1..endpoint::RATES]
                .iter()
                .any(|b| *b != 0)
        {
            return Err(Errno::BadMagic);
        }
        let mut name = [0u8; AUDIO_NAME_MAX];
        name.copy_from_slice(&bytes[endpoint::NAME..endpoint::LEN]);
        let facts = Self {
            index: read_u16(bytes, endpoint::INDEX),
            direction: StreamDirection::from_u8(bytes[endpoint::DIRECTION])?,
            jack: JackState::from_u8(bytes[endpoint::JACK])?,
            formats: SampleFormats::from_bits(read_u16(bytes, endpoint::FORMATS))?,
            channel_map: ChannelMap::from_wire(&bytes[endpoint::CHANNEL_MAP..])?,
            rates: RateSupport::from_wire(&bytes[endpoint::RATES..])?,
            min_period_frames: read_u32(bytes, endpoint::MIN_PERIOD),
            max_period_frames: read_u32(bytes, endpoint::MAX_PERIOD),
            max_ring_frames: read_u32(bytes, endpoint::MAX_RING),
            gain: GainRange::from_wire(&bytes[endpoint::GAIN..])?,
            name: AudioName::from_wire(bytes[endpoint::NAME_LEN], &name)?,
        };
        facts.validate()?;
        Ok(facts)
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
