//! Bitstreams the VP8 tests and the container's tests are written from.
//!
//! The boolean encoder is the reference *encoder* from the format's own
//! specification rather than the inverse of the decoder beside it, so a
//! round trip tests the decoder against the format. The tree writer finds
//! each leaf's path from the same tree arrays the decoder walks, which is
//! the one thing a fixture cannot usefully re-derive.

use alloc::vec::Vec;

use super::{
    BMODE_TREE, COEFF_BANDS, COEFF_TREE, COEFF_UPDATE_PROBS, DEFAULT_COEFF_PROBS, KF_BMODE_PROBS,
    KF_UV_MODE_PROBS, KF_YMODE_PROBS, KF_YMODE_TREE, START_CODE, UV_MODE_TREE,
};

/// The boolean entropy encoder from the format's own specification.
pub(crate) struct Writer {
    pub(crate) out: Vec<u8>,
    pub(crate) range: u32,
    pub(crate) bottom: u32,
    pub(crate) count: i32,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self {
            out: Vec::new(),
            range: 255,
            bottom: 0,
            count: 24,
        }
    }

    /// Propagate a carry into the bytes already written.
    pub(crate) fn carry(&mut self) {
        for byte in self.out.iter_mut().rev() {
            if *byte == u8::MAX {
                *byte = 0;
            } else {
                *byte += 1;
                return;
            }
        }
    }

    pub(crate) fn bit(&mut self, probability: u8, value: bool) {
        let split = 1 + (((self.range - 1) * u32::from(probability)) >> 8);
        if value {
            self.bottom = self.bottom.wrapping_add(split);
            self.range -= split;
        } else {
            self.range = split;
        }
        while self.range < 128 {
            self.range <<= 1;
            if self.bottom & (1 << 31) != 0 {
                self.carry();
            }
            self.bottom = self.bottom.wrapping_shl(1);
            self.count -= 1;
            if self.count == 0 {
                self.out
                    .push(u8::try_from(self.bottom >> 24).expect("one byte"));
                self.bottom &= (1 << 24) - 1;
                self.count = 8;
            }
        }
    }

    pub(crate) fn flag(&mut self, value: bool) {
        self.bit(128, value);
    }

    /// An unsigned literal, most significant bit first.
    pub(crate) fn literal(&mut self, value: u32, bits: u32) {
        for index in (0..bits).rev() {
            self.flag((value >> index) & 1 != 0);
        }
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        let count = self.count;
        let mut value = self.bottom;
        if count < 32 && value & (1u32.wrapping_shl(u32::try_from(32 - count).unwrap_or(0))) != 0 {
            self.carry();
        }
        value = value.wrapping_shl(u32::try_from(count & 7).unwrap_or(0));
        for _ in 0..(count >> 3) {
            value = value.wrapping_shl(8);
        }
        for _ in 0..4 {
            self.out.push(u8::try_from(value >> 24).expect("one byte"));
            value = value.wrapping_shl(8);
        }
        self.out
    }
}

/// A deterministic generator, so a round-trip sweep is reproducible.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u32::try_from(self.0 >> 33).unwrap_or(0)
    }
}

/// The path from a tree's root to the leaf carrying `value`, as the
/// (probability index, branch) pairs a writer must emit.
pub(crate) fn path(tree: &[i8], node: usize, value: usize, out: &mut Vec<(usize, bool)>) -> bool {
    for branch in 0..2 {
        let next = tree[node + branch];
        out.push((node >> 1, branch == 1));
        if next <= 0 && usize::try_from(-i32::from(next)).unwrap_or(usize::MAX) == value {
            return true;
        }
        if next > 0
            && path(
                tree,
                usize::try_from(next).expect("a node index"),
                value,
                out,
            )
        {
            return true;
        }
        out.pop();
    }
    false
}

/// Write the tree path that decodes to `value`.
pub(crate) fn write_tree(writer: &mut Writer, tree: &[i8], probs: &[u8], value: usize) {
    let mut steps = Vec::new();
    assert!(path(tree, 0, value, &mut steps), "the tree holds {value}");
    for (index, branch) in steps {
        writer.bit(probs[index], branch);
    }
}

/// The token values the coefficient tree names.
pub(crate) const DCT_EOB: usize = 11;

/// The whole-macroblock luma mode that gives every subblock its own.
pub(crate) const B_PRED: usize = 4;

/// What one macroblock is coded as.
#[derive(Copy, Clone)]
pub(crate) struct Block {
    pub(crate) ymode: usize,
    pub(crate) uvmode: usize,
    /// The mode every subblock takes when the luma mode is [`B_PRED`].
    pub(crate) bmode: usize,
    /// The luma DC block's first coefficient, as a small token value.
    pub(crate) y2_dc: usize,
}

impl Block {
    pub(crate) const fn flat(ymode: usize, uvmode: usize) -> Self {
        Self {
            ymode,
            uvmode,
            bmode: 0,
            y2_dc: 0,
        }
    }

    /// A macroblock whose sixteen subblocks each take `bmode`.
    pub(crate) const fn subblocks(bmode: usize) -> Self {
        Self {
            ymode: B_PRED,
            uvmode: 0,
            bmode,
            y2_dc: 0,
        }
    }
}

/// Write a whole one-macroblock keyframe.
pub(crate) fn keyframe(width: u32, height: u32, block: Block) -> Vec<u8> {
    let mut head = Writer::new();
    head.flag(false);
    head.flag(false);
    head.flag(false);
    head.flag(false);
    head.literal(0, 6);
    head.literal(0, 3);
    head.flag(false);
    head.literal(0, 2);
    head.literal(0, 7);
    for _ in 0..5 {
        head.flag(false);
    }
    head.flag(true);
    for plane in &COEFF_UPDATE_PROBS {
        for band in plane {
            for context in band {
                for &update in context {
                    head.bit(update, false);
                }
            }
        }
    }
    head.flag(false);
    write_tree(&mut head, &KF_YMODE_TREE, &KF_YMODE_PROBS, block.ymode);
    if block.ymode == B_PRED {
        // A subblock mode is read against the modes of the subblocks above
        // and to its left, which outside the macroblock are the averaging
        // mode: so the first row's above context and the first column's
        // left context are that, and the rest is the mode being written.
        let mut above = [0usize; 4];
        for _ in 0..4 {
            let mut left = 0usize;
            for column in 0..4 {
                let table = &KF_BMODE_PROBS[above[column]][left];
                write_tree(&mut head, &BMODE_TREE, table, block.bmode);
                left = block.bmode;
                above[column] = block.bmode;
            }
        }
    }
    write_tree(&mut head, &UV_MODE_TREE, &KF_UV_MODE_PROBS, block.uvmode);
    let first_part = head.finish();

    let mut tokens = Writer::new();
    let probs = &DEFAULT_COEFF_PROBS;
    let band_at = |scan: usize| usize::from(COEFF_BANDS[scan]);
    // A subblock-predicted macroblock carries no luma DC block, so its luma
    // blocks start at coefficient zero and use their own plane.
    if block.ymode == B_PRED {
        for _ in 0..16 {
            write_tree(&mut tokens, &COEFF_TREE, &probs[3][band_at(0)][0], DCT_EOB);
        }
    } else if block.y2_dc == 0 {
        write_tree(&mut tokens, &COEFF_TREE, &probs[1][band_at(0)][0], DCT_EOB);
    } else {
        write_tree(
            &mut tokens,
            &COEFF_TREE,
            &probs[1][band_at(0)][0],
            block.y2_dc,
        );
        tokens.flag(false);
        let context = if block.y2_dc == 1 { 1 } else { 2 };
        write_tree(
            &mut tokens,
            &COEFF_TREE,
            &probs[1][band_at(1)][context],
            DCT_EOB,
        );
    }
    if block.ymode != B_PRED {
        for _ in 0..16 {
            write_tree(&mut tokens, &COEFF_TREE, &probs[0][band_at(1)][0], DCT_EOB);
        }
    }
    for _ in 0..8 {
        write_tree(&mut tokens, &COEFF_TREE, &probs[2][band_at(0)][0], DCT_EOB);
    }
    let residual = tokens.finish();

    let mut bytes = Vec::new();
    let tag = u32::try_from(first_part.len()).expect("a small partition") << 5;
    bytes.push(u8::try_from(tag & 0xFF).expect("one byte"));
    bytes.push(u8::try_from((tag >> 8) & 0xFF).expect("one byte"));
    bytes.push(u8::try_from((tag >> 16) & 0xFF).expect("one byte"));
    bytes.extend_from_slice(&START_CODE);
    bytes.extend_from_slice(&u16::try_from(width).expect("a small width").to_le_bytes());
    bytes.extend_from_slice(&u16::try_from(height).expect("a small height").to_le_bytes());
    bytes.extend_from_slice(&first_part);
    bytes.extend_from_slice(&residual);
    bytes
}
