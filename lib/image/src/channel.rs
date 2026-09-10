//! One colour channel's bits within a packed pixel, and the sampler that
//! widens a channel's raw value to eight bits.
//!
//! Shared by every format whose pixels are a little-endian value with the
//! channels cut out of it — a BMP's bitfields, a RISC OS sprite's `TBGR`
//! word — because the cutting and the widening are the same arithmetic
//! whether the field positions were read from the file or fixed by the
//! format.

/// One colour channel's bits within a packed pixel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Channel {
    mask: u32,
    shift: u32,
    width: u32,
}

impl Channel {
    /// A channel the pixel does not carry.
    pub(crate) const ABSENT: Self = Self {
        mask: 0,
        shift: 0,
        width: 0,
    };

    /// A channel at a position the format fixes, so there is nothing to
    /// refuse: `width` bits starting at `shift`.
    ///
    /// A width or position past the top of the value yields [`Self::ABSENT`]
    /// rather than a wrapped mask, which keeps the constructor total for a
    /// caller whose arithmetic produced one.
    pub(crate) const fn fixed(shift: u32, width: u32) -> Self {
        if width == 0 || width > u32::BITS || shift >= u32::BITS || shift + width > u32::BITS {
            return Self::ABSENT;
        }
        Self {
            mask: (u32::MAX >> (u32::BITS - width)) << shift,
            shift,
            width,
        }
    }

    /// Whether the pixel carries this channel at all.
    pub(crate) const fn present(self) -> bool {
        self.width != 0
    }

    /// A channel from a mask a file declared, or `None` where its set bits
    /// are not contiguous: a scattered field names no single sample value.
    ///
    /// The refusal is the caller's to name, because a mask is only ever read
    /// from a file whose own error vocabulary describes it.
    pub(crate) fn new(mask: u32) -> Option<Self> {
        if mask == 0 {
            return Some(Self::ABSENT);
        }
        let shift = mask.trailing_zeros();
        let width = mask.count_ones();
        (mask >> shift == u32::MAX >> (u32::BITS - width)).then_some(Self { mask, shift, width })
    }
}

/// A channel's raw values pre-scaled to eight bits.
///
/// Widening a channel is `raw * 255 / max`, so a megapixel image would pay
/// four divisions per pixel to do it as it goes; tabulating costs 256
/// divisions per channel per decode instead. A channel wider than eight bits
/// is truncated to its top eight, which is exact at both ends of its range,
/// so that extra shift folds into the sampler's own.
pub(crate) struct Sampler {
    mask: u32,
    shift: u32,
    table: [u8; 256],
}

impl Sampler {
    /// An absent channel is opaque, which is right because alpha is the only
    /// channel that can be absent: a zero mask always reads value zero, so a
    /// saturated table answers full for every pixel with no branch.
    pub(crate) fn new(channel: Channel) -> Self {
        if channel.width == 0 {
            return Self {
                mask: 0,
                shift: 0,
                table: [u8::MAX; 256],
            };
        }
        let max = u32::from(u8::MAX >> (8 - channel.width.min(8)));
        let mut table = [0u8; 256];
        for (raw, entry) in (0..).zip(table.iter_mut()) {
            if raw > max {
                break;
            }
            *entry = u8::try_from((raw * 255 + max / 2) / max).unwrap_or(u8::MAX);
        }
        Self {
            mask: channel.mask,
            shift: channel.shift + channel.width.saturating_sub(8),
            table,
        }
    }

    pub(crate) fn sample(&self, raw: u32) -> u8 {
        let value = u8::try_from((raw & self.mask) >> self.shift & 0xFF).unwrap_or(0);
        self.table[usize::from(value)]
    }
}
