//! A complete, fail-closed VP8 keyframe decoder (RFC 6386).
//!
//! The WEBP container carries exactly one kind of VP8 frame — a keyframe —
//! so everything here is intra coding: there are no reference frames, no
//! motion vectors, and no inter modes, and a bitstream that declares itself
//! an interframe is refused rather than half-read. Within that, the format
//! is complete: the boolean entropy decoder, the segmentation, loop-filter
//! and quantiser headers with their per-segment and per-mode deltas, the
//! token probability updates, the four whole-macroblock luma modes and four
//! chroma modes, all ten subblock modes, the Walsh-Hadamard and DCT
//! reconstructions, both the normal and the simple loop filter at
//! macroblock and subblock edges, and the conversion to RGB.
//!
//! # Two readings of the frame header
//!
//! **A reserved colour space is refused.** The header's one colour-space bit
//! selects ITU-R BT.601 at zero and is reserved at one, and a decoder that
//! guessed BT.601 for the reserved value would silently produce the wrong
//! colours rather than say it could not.
//!
//! **The clamping-type bit is read and not acted on.** It says whether
//! reconstruction *needs* clamping; clamping regardless is correct either
//! way, and the alternative is trusting a file's word that its arithmetic
//! stays in range.
//!
//! The horizontal and vertical scale fields are likewise read and not acted
//! on: they ask a *display* to stretch the picture, and the decoded picture
//! is the size the header declares.
//!
//! # Where the arithmetic must match exactly
//!
//! Both transforms, the six intra-prediction averaging rules, and both loop
//! filters are specified as integer code with exact rounding, and a decoder
//! that rounds differently drifts visibly rather than approximately. Each is
//! transcribed from the reference decoder's own arithmetic.
//!
//! One erratum is worth naming: the reference code for the horizontal-down
//! subblock mode reads `svg2p` where the mode's own diagonal pattern, and
//! every other implementation, require `avg2p`.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::{DecodeError, DecodeLimits, RasterImage, RGBA_BYTES};

/// The three bytes a keyframe's uncompressed header carries after its frame
/// tag (RFC 6386 §9.1).
const START_CODE: [u8; 3] = [0x9D, 0x01, 0x2A];

/// Bytes a keyframe's uncompressed header occupies: the frame tag, the start
/// code, and the two dimensions.
const UNCOMPRESSED_HEADER: usize = 10;

/// Segments a frame may divide its macroblocks into.
const SEGMENTS: usize = 4;

/// Coefficient plane types the token probabilities are indexed by: luma
/// after a Y2 block, the Y2 block, chroma, and luma with no Y2 block.
const PLANES: usize = 4;

/// Coefficient bands one plane's probabilities are grouped into.
const BANDS: usize = 8;

/// Previous-coefficient contexts a band's probabilities are chosen by.
const CONTEXTS: usize = 3;

/// Probabilities one context holds, one per interior node of the token tree.
const NODES: usize = 11;

/// Residual partitions a frame may split its tokens across.
const MAX_PARTITIONS: usize = 8;

/// Coefficients one transform block holds.
const BLOCK_COEFFS: usize = 16;

/// Transform blocks one macroblock holds: sixteen luma, four each of two
/// chroma planes, and the luma DC block.
const MB_BLOCKS: usize = 25;

/// The luma DC block's position among a macroblock's blocks.
const Y2_BLOCK: usize = 24;

/// Whole-macroblock luma and chroma modes, in the order the trees name them.
/// The first is the mode every other arm falls through to, so only the
/// three that branch are named.
const V_PRED: usize = 1;
const H_PRED: usize = 2;
const TM_PRED: usize = 3;
const B_PRED: usize = 4;

/// Subblock modes, in the order the tree names them.
const B_DC_PRED: usize = 0;
const B_TM_PRED: usize = 1;
const B_VE_PRED: usize = 2;
const B_HE_PRED: usize = 3;
const B_LD_PRED: usize = 4;
const B_RD_PRED: usize = 5;
const B_VR_PRED: usize = 6;
const B_VL_PRED: usize = 7;
const B_HD_PRED: usize = 8;
const B_HU_PRED: usize = 9;

/// The subblock mode a whole-macroblock mode implies for a neighbour's
/// context (RFC 6386 §11.3).
const MODE_TO_BMODE: [usize; 4] = [B_DC_PRED, B_VE_PRED, B_HE_PRED, B_TM_PRED];

/// The value an absent above row predicts from.
const ABOVE_ABSENT: u8 = 127;

/// The value an absent left column predicts from.
const LEFT_ABSENT: u8 = 129;

/// Token values the coefficient tree names.
const DCT_0: usize = 0;
const DCT_CAT1: usize = 5;
const DCT_EOB: usize = 11;

/// The node the coefficient tree's end-of-block branch sits above, which a
/// read following a zero coefficient starts below.
const TREE_AFTER_EOB: usize = 2;

/// Extra-bit probabilities for each large-coefficient category, terminated
/// by the zero the reference decoder's loop stops on (RFC 6386 §13.2).
const CATEGORY_PROBS: [&[u8]; 6] = [
    &[159],
    &[165, 145],
    &[173, 148, 140, 135],
    &[176, 155, 140, 135],
    &[180, 157, 141, 134, 130],
    &[254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129],
];

/// The smallest absolute value each large-coefficient category names.
const CATEGORY_BASES: [i32; 6] = [5, 7, 11, 19, 35, 67];

/// The two 16-bit fixed-point multipliers the inverse DCT uses.
const COS_PI8_SQRT2_MINUS1: i32 = 20091;
const SIN_PI8_SQRT2: i32 = 35468;

/// The DC dequantisation factor each quantiser index names
/// (RFC 6386 §14.1).
const DC_QLOOKUP: [i32; 128] = [
    4, 5, 6, 7, 8, 9, 10, 10, 11, 12, 13, 14, 15, 16, 17, 17, 18, 19, 20, 20, 21, 21, 22, 22, 23,
    23, 24, 25, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 37, 38, 39, 40, 41, 42, 43, 44,
    45, 46, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67,
    68, 69, 70, 71, 72, 73, 74, 75, 76, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 91,
    93, 95, 96, 98, 100, 101, 102, 104, 106, 108, 110, 112, 114, 116, 118, 122, 124, 126, 128, 130,
    132, 134, 136, 138, 140, 143, 145, 148, 151, 154, 157,
];

/// The AC dequantisation factor each quantiser index names.
const AC_QLOOKUP: [i32; 128] = [
    4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
    29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52,
    53, 54, 55, 56, 57, 58, 60, 62, 64, 66, 68, 70, 72, 74, 76, 78, 80, 82, 84, 86, 88, 90, 92, 94,
    96, 98, 100, 102, 104, 106, 108, 110, 112, 114, 116, 119, 122, 125, 128, 131, 134, 137, 140,
    143, 146, 149, 152, 155, 158, 161, 164, 167, 170, 173, 177, 181, 185, 189, 193, 197, 201, 205,
    209, 213, 217, 221, 225, 229, 234, 239, 245, 249, 254, 259, 264, 269, 274, 279, 284,
];

/// The position in a transform block each scan index writes to.
const ZIGZAG: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// The probability band each scan index draws its token
/// probabilities from.
const COEFF_BANDS: [u8; 16] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7];

/// The coefficient token tree: a positive entry is the next node, a
/// non-positive one is the negated token it decodes to.
const COEFF_TREE: [i8; 22] = [
    -11, 2, 0, 4, -1, 6, 8, 12, -2, 10, -3, -4, 14, 16, -5, -6, 18, 20, -7, -8, -9, -10,
];

/// The subblock-mode tree.
const BMODE_TREE: [i8; 18] = [
    0, 2, -1, 4, -2, 6, 8, 12, -3, 10, -5, -6, -4, 14, -7, 16, -8, -9,
];

/// The whole-macroblock luma-mode tree a keyframe uses.
const KF_YMODE_TREE: [i8; 8] = [-4, 2, 4, 6, 0, -1, -2, -3];

/// The chroma-mode tree.
const UV_MODE_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];

/// The probabilities the keyframe luma-mode tree is read with.
const KF_YMODE_PROBS: [u8; 4] = [145, 156, 163, 128];

/// The probabilities the chroma-mode tree is read with.
const KF_UV_MODE_PROBS: [u8; 3] = [142, 114, 183];

/// The probabilities a subblock mode is read with, chosen by the modes
/// of the subblocks above and to the left (RFC 6386 §11.4).
const KF_BMODE_PROBS: [[[u8; 9]; 10]; 10] = [
    [
        [231, 120, 48, 89, 115, 113, 120, 152, 112],
        [152, 179, 64, 126, 170, 118, 46, 70, 95],
        [175, 69, 143, 80, 85, 82, 72, 155, 103],
        [56, 58, 10, 171, 218, 189, 17, 13, 152],
        [144, 71, 10, 38, 171, 213, 144, 34, 26],
        [114, 26, 17, 163, 44, 195, 21, 10, 173],
        [121, 24, 80, 195, 26, 62, 44, 64, 85],
        [170, 46, 55, 19, 136, 160, 33, 206, 71],
        [63, 20, 8, 114, 114, 208, 12, 9, 226],
        [81, 40, 11, 96, 182, 84, 29, 16, 36],
    ],
    [
        [134, 183, 89, 137, 98, 101, 106, 165, 148],
        [72, 187, 100, 130, 157, 111, 32, 75, 80],
        [66, 102, 167, 99, 74, 62, 40, 234, 128],
        [41, 53, 9, 178, 241, 141, 26, 8, 107],
        [104, 79, 12, 27, 217, 255, 87, 17, 7],
        [74, 43, 26, 146, 73, 166, 49, 23, 157],
        [65, 38, 105, 160, 51, 52, 31, 115, 128],
        [87, 68, 71, 44, 114, 51, 15, 186, 23],
        [47, 41, 14, 110, 182, 183, 21, 17, 194],
        [66, 45, 25, 102, 197, 189, 23, 18, 22],
    ],
    [
        [88, 88, 147, 150, 42, 46, 45, 196, 205],
        [43, 97, 183, 117, 85, 38, 35, 179, 61],
        [39, 53, 200, 87, 26, 21, 43, 232, 171],
        [56, 34, 51, 104, 114, 102, 29, 93, 77],
        [107, 54, 32, 26, 51, 1, 81, 43, 31],
        [39, 28, 85, 171, 58, 165, 90, 98, 64],
        [34, 22, 116, 206, 23, 34, 43, 166, 73],
        [68, 25, 106, 22, 64, 171, 36, 225, 114],
        [34, 19, 21, 102, 132, 188, 16, 76, 124],
        [62, 18, 78, 95, 85, 57, 50, 48, 51],
    ],
    [
        [193, 101, 35, 159, 215, 111, 89, 46, 111],
        [60, 148, 31, 172, 219, 228, 21, 18, 111],
        [112, 113, 77, 85, 179, 255, 38, 120, 114],
        [40, 42, 1, 196, 245, 209, 10, 25, 109],
        [100, 80, 8, 43, 154, 1, 51, 26, 71],
        [88, 43, 29, 140, 166, 213, 37, 43, 154],
        [61, 63, 30, 155, 67, 45, 68, 1, 209],
        [142, 78, 78, 16, 255, 128, 34, 197, 171],
        [41, 40, 5, 102, 211, 183, 4, 1, 221],
        [51, 50, 17, 168, 209, 192, 23, 25, 82],
    ],
    [
        [125, 98, 42, 88, 104, 85, 117, 175, 82],
        [95, 84, 53, 89, 128, 100, 113, 101, 45],
        [75, 79, 123, 47, 51, 128, 81, 171, 1],
        [57, 17, 5, 71, 102, 57, 53, 41, 49],
        [115, 21, 2, 10, 102, 255, 166, 23, 6],
        [38, 33, 13, 121, 57, 73, 26, 1, 85],
        [41, 10, 67, 138, 77, 110, 90, 47, 114],
        [101, 29, 16, 10, 85, 128, 101, 196, 26],
        [57, 18, 10, 102, 102, 213, 34, 20, 43],
        [117, 20, 15, 36, 163, 128, 68, 1, 26],
    ],
    [
        [138, 31, 36, 171, 27, 166, 38, 44, 229],
        [67, 87, 58, 169, 82, 115, 26, 59, 179],
        [63, 59, 90, 180, 59, 166, 93, 73, 154],
        [40, 40, 21, 116, 143, 209, 34, 39, 175],
        [57, 46, 22, 24, 128, 1, 54, 17, 37],
        [47, 15, 16, 183, 34, 223, 49, 45, 183],
        [46, 17, 33, 183, 6, 98, 15, 32, 183],
        [65, 32, 73, 115, 28, 128, 23, 128, 205],
        [40, 3, 9, 115, 51, 192, 18, 6, 223],
        [87, 37, 9, 115, 59, 77, 64, 21, 47],
    ],
    [
        [104, 55, 44, 218, 9, 54, 53, 130, 226],
        [64, 90, 70, 205, 40, 41, 23, 26, 57],
        [54, 57, 112, 184, 5, 41, 38, 166, 213],
        [30, 34, 26, 133, 152, 116, 10, 32, 134],
        [75, 32, 12, 51, 192, 255, 160, 43, 51],
        [39, 19, 53, 221, 26, 114, 32, 73, 255],
        [31, 9, 65, 234, 2, 15, 1, 118, 73],
        [88, 31, 35, 67, 102, 85, 55, 186, 85],
        [56, 21, 23, 111, 59, 205, 45, 37, 192],
        [55, 38, 70, 124, 73, 102, 1, 34, 98],
    ],
    [
        [102, 61, 71, 37, 34, 53, 31, 243, 192],
        [69, 60, 71, 38, 73, 119, 28, 222, 37],
        [68, 45, 128, 34, 1, 47, 11, 245, 171],
        [62, 17, 19, 70, 146, 85, 55, 62, 70],
        [75, 15, 9, 9, 64, 255, 184, 119, 16],
        [37, 43, 37, 154, 100, 163, 85, 160, 1],
        [63, 9, 92, 136, 28, 64, 32, 201, 85],
        [86, 6, 28, 5, 64, 255, 25, 248, 1],
        [56, 8, 17, 132, 137, 255, 55, 116, 128],
        [58, 15, 20, 82, 135, 57, 26, 121, 40],
    ],
    [
        [164, 50, 31, 137, 154, 133, 25, 35, 218],
        [51, 103, 44, 131, 131, 123, 31, 6, 158],
        [86, 40, 64, 135, 148, 224, 45, 183, 128],
        [22, 26, 17, 131, 240, 154, 14, 1, 209],
        [83, 12, 13, 54, 192, 255, 68, 47, 28],
        [45, 16, 21, 91, 64, 222, 7, 1, 197],
        [56, 21, 39, 155, 60, 138, 23, 102, 213],
        [85, 26, 85, 85, 128, 128, 32, 146, 171],
        [18, 11, 7, 63, 144, 171, 4, 4, 246],
        [35, 27, 10, 146, 174, 171, 12, 26, 128],
    ],
    [
        [190, 80, 35, 99, 180, 80, 126, 54, 45],
        [85, 126, 47, 87, 176, 51, 41, 20, 32],
        [101, 75, 128, 139, 118, 146, 116, 128, 85],
        [56, 41, 15, 176, 236, 85, 37, 9, 62],
        [146, 36, 19, 30, 171, 255, 97, 27, 20],
        [71, 30, 17, 119, 118, 255, 17, 18, 138],
        [101, 38, 60, 138, 55, 70, 43, 26, 142],
        [138, 45, 61, 62, 219, 1, 81, 188, 64],
        [32, 41, 20, 117, 151, 142, 20, 21, 163],
        [112, 19, 12, 61, 195, 128, 48, 4, 24],
    ],
];

/// The token probabilities a keyframe starts from (RFC 6386 §13.5).
///
/// Plane zero's first band is unused — a luma block following a Y2 block
/// starts at scan index one, whose band is one — so its placeholder
/// entries are never read.
const DEFAULT_COEFF_PROBS: [[[[u8; 11]; 3]; 8]; 4] = [
    [
        [
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [253, 136, 254, 255, 228, 219, 128, 128, 128, 128, 128],
            [189, 129, 242, 255, 227, 213, 255, 219, 128, 128, 128],
            [106, 126, 227, 252, 214, 209, 255, 255, 128, 128, 128],
        ],
        [
            [1, 98, 248, 255, 236, 226, 255, 255, 128, 128, 128],
            [181, 133, 238, 254, 221, 234, 255, 154, 128, 128, 128],
            [78, 134, 202, 247, 198, 180, 255, 219, 128, 128, 128],
        ],
        [
            [1, 185, 249, 255, 243, 255, 128, 128, 128, 128, 128],
            [184, 150, 247, 255, 236, 224, 128, 128, 128, 128, 128],
            [77, 110, 216, 255, 236, 230, 128, 128, 128, 128, 128],
        ],
        [
            [1, 101, 251, 255, 241, 255, 128, 128, 128, 128, 128],
            [170, 139, 241, 252, 236, 209, 255, 255, 128, 128, 128],
            [37, 116, 196, 243, 228, 255, 255, 255, 128, 128, 128],
        ],
        [
            [1, 204, 254, 255, 245, 255, 128, 128, 128, 128, 128],
            [207, 160, 250, 255, 238, 128, 128, 128, 128, 128, 128],
            [102, 103, 231, 255, 211, 171, 128, 128, 128, 128, 128],
        ],
        [
            [1, 152, 252, 255, 240, 255, 128, 128, 128, 128, 128],
            [177, 135, 243, 255, 234, 225, 128, 128, 128, 128, 128],
            [80, 129, 211, 255, 194, 224, 128, 128, 128, 128, 128],
        ],
        [
            [1, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [246, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [255, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
    ],
    [
        [
            [198, 35, 237, 223, 193, 187, 162, 160, 145, 155, 62],
            [131, 45, 198, 221, 172, 176, 220, 157, 252, 221, 1],
            [68, 47, 146, 208, 149, 167, 221, 162, 255, 223, 128],
        ],
        [
            [1, 149, 241, 255, 221, 224, 255, 255, 128, 128, 128],
            [184, 141, 234, 253, 222, 220, 255, 199, 128, 128, 128],
            [81, 99, 181, 242, 176, 190, 249, 202, 255, 255, 128],
        ],
        [
            [1, 129, 232, 253, 214, 197, 242, 196, 255, 255, 128],
            [99, 121, 210, 250, 201, 198, 255, 202, 128, 128, 128],
            [23, 91, 163, 242, 170, 187, 247, 210, 255, 255, 128],
        ],
        [
            [1, 200, 246, 255, 234, 255, 128, 128, 128, 128, 128],
            [109, 178, 241, 255, 231, 245, 255, 255, 128, 128, 128],
            [44, 130, 201, 253, 205, 192, 255, 255, 128, 128, 128],
        ],
        [
            [1, 132, 239, 251, 219, 209, 255, 165, 128, 128, 128],
            [94, 136, 225, 251, 218, 190, 255, 255, 128, 128, 128],
            [22, 100, 174, 245, 186, 161, 255, 199, 128, 128, 128],
        ],
        [
            [1, 182, 249, 255, 232, 235, 128, 128, 128, 128, 128],
            [124, 143, 241, 255, 227, 234, 128, 128, 128, 128, 128],
            [35, 77, 181, 251, 193, 211, 255, 205, 128, 128, 128],
        ],
        [
            [1, 157, 247, 255, 236, 231, 255, 255, 128, 128, 128],
            [121, 141, 235, 255, 225, 227, 255, 255, 128, 128, 128],
            [45, 99, 188, 251, 195, 217, 255, 224, 128, 128, 128],
        ],
        [
            [1, 1, 251, 255, 213, 255, 128, 128, 128, 128, 128],
            [203, 1, 248, 255, 255, 128, 128, 128, 128, 128, 128],
            [137, 1, 177, 255, 224, 255, 128, 128, 128, 128, 128],
        ],
    ],
    [
        [
            [253, 9, 248, 251, 207, 208, 255, 192, 128, 128, 128],
            [175, 13, 224, 243, 193, 185, 249, 198, 255, 255, 128],
            [73, 17, 171, 221, 161, 179, 236, 167, 255, 234, 128],
        ],
        [
            [1, 95, 247, 253, 212, 183, 255, 255, 128, 128, 128],
            [239, 90, 244, 250, 211, 209, 255, 255, 128, 128, 128],
            [155, 77, 195, 248, 188, 195, 255, 255, 128, 128, 128],
        ],
        [
            [1, 24, 239, 251, 218, 219, 255, 205, 128, 128, 128],
            [201, 51, 219, 255, 196, 186, 128, 128, 128, 128, 128],
            [69, 46, 190, 239, 201, 218, 255, 228, 128, 128, 128],
        ],
        [
            [1, 191, 251, 255, 255, 128, 128, 128, 128, 128, 128],
            [223, 165, 249, 255, 213, 255, 128, 128, 128, 128, 128],
            [141, 124, 248, 255, 255, 128, 128, 128, 128, 128, 128],
        ],
        [
            [1, 16, 248, 255, 255, 128, 128, 128, 128, 128, 128],
            [190, 36, 230, 255, 236, 255, 128, 128, 128, 128, 128],
            [149, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [1, 226, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [247, 192, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [240, 128, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [1, 134, 252, 255, 255, 128, 128, 128, 128, 128, 128],
            [213, 62, 250, 255, 255, 128, 128, 128, 128, 128, 128],
            [55, 93, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
        [
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
            [128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
    ],
    [
        [
            [202, 24, 213, 235, 186, 191, 220, 160, 240, 175, 255],
            [126, 38, 182, 232, 169, 184, 228, 174, 255, 187, 128],
            [61, 46, 138, 219, 151, 178, 240, 170, 255, 216, 128],
        ],
        [
            [1, 112, 230, 250, 199, 191, 247, 159, 255, 255, 128],
            [166, 109, 228, 252, 211, 215, 255, 174, 128, 128, 128],
            [39, 77, 162, 232, 172, 180, 245, 178, 255, 255, 128],
        ],
        [
            [1, 52, 220, 246, 198, 199, 249, 220, 255, 255, 128],
            [124, 74, 191, 243, 183, 193, 250, 221, 255, 255, 128],
            [24, 71, 130, 219, 154, 170, 243, 182, 255, 255, 128],
        ],
        [
            [1, 182, 225, 249, 219, 240, 255, 224, 128, 128, 128],
            [149, 150, 226, 252, 216, 205, 255, 171, 128, 128, 128],
            [28, 108, 170, 242, 183, 194, 254, 223, 255, 255, 128],
        ],
        [
            [1, 81, 230, 252, 204, 203, 255, 192, 128, 128, 128],
            [123, 102, 209, 247, 188, 196, 255, 233, 128, 128, 128],
            [20, 95, 153, 243, 164, 173, 255, 203, 128, 128, 128],
        ],
        [
            [1, 222, 248, 255, 216, 213, 128, 128, 128, 128, 128],
            [168, 175, 246, 252, 235, 205, 255, 255, 128, 128, 128],
            [47, 116, 215, 255, 211, 212, 255, 255, 128, 128, 128],
        ],
        [
            [1, 121, 236, 253, 212, 214, 255, 255, 128, 128, 128],
            [141, 84, 213, 252, 201, 202, 255, 219, 128, 128, 128],
            [42, 80, 160, 240, 162, 185, 255, 205, 128, 128, 128],
        ],
        [
            [1, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [244, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
            [238, 1, 255, 128, 128, 128, 128, 128, 128, 128, 128],
        ],
    ],
];

/// The probability that each token probability is replaced by one the
/// frame header carries (RFC 6386 §13.4).
const COEFF_UPDATE_PROBS: [[[[u8; 11]; 3]; 8]; 4] = [
    [
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [176, 246, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [223, 241, 252, 255, 255, 255, 255, 255, 255, 255, 255],
            [249, 253, 253, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 244, 252, 255, 255, 255, 255, 255, 255, 255, 255],
            [234, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 246, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [239, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 248, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [251, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [251, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 253, 255, 254, 255, 255, 255, 255, 255, 255],
            [250, 255, 254, 255, 254, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
    [
        [
            [217, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [225, 252, 241, 253, 255, 255, 254, 255, 255, 255, 255],
            [234, 250, 241, 250, 253, 255, 253, 254, 255, 255, 255],
        ],
        [
            [255, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [223, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [238, 253, 254, 254, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 248, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [249, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [247, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [252, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [250, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
    [
        [
            [186, 251, 250, 255, 255, 255, 255, 255, 255, 255, 255],
            [234, 251, 244, 254, 255, 255, 255, 255, 255, 255, 255],
            [251, 251, 243, 253, 254, 255, 254, 255, 255, 255, 255],
        ],
        [
            [255, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [236, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [251, 253, 253, 254, 254, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 254, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
    [
        [
            [248, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [250, 254, 252, 254, 255, 255, 255, 255, 255, 255, 255],
            [248, 254, 249, 253, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 253, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [246, 253, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [252, 254, 251, 254, 254, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 254, 252, 255, 255, 255, 255, 255, 255, 255, 255],
            [248, 254, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 255, 254, 254, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 251, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [245, 251, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [253, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 251, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [252, 253, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 254, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 252, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [249, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 254, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 253, 255, 255, 255, 255, 255, 255, 255, 255],
            [250, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
        [
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [254, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ],
    ],
];

/// A boolean entropy decoder (RFC 6386 §7.3).
///
/// A read past the end of the partition yields zero bytes and records that
/// it happened, rather than refusing on the spot: the decoder's two-byte
/// lookahead legitimately reaches past the last meaningful byte, so only a
/// frame that *finished* short is malformed. [`Self::exhausted`] is checked
/// once the frame is decoded, and no pixels are handed out when it is set.
struct Bool<'a> {
    bytes: &'a [u8],
    next: usize,
    value: u32,
    range: u32,
    count: u32,
    exhausted: bool,
}

impl<'a> Bool<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let mut decoder = Self {
            bytes,
            next: 0,
            value: 0,
            range: 255,
            count: 0,
            exhausted: false,
        };
        decoder.value = (u32::from(decoder.byte()) << 8) | u32::from(decoder.byte());
        decoder
    }

    fn byte(&mut self) -> u8 {
        let Some(&value) = self.bytes.get(self.next) else {
            self.exhausted = true;
            return 0;
        };
        self.next += 1;
        value
    }

    fn exhausted(&self) -> bool {
        self.exhausted
    }

    fn bit(&mut self, probability: u8) -> u32 {
        let split = 1 + (((self.range - 1) * u32::from(probability)) >> 8);
        let big_split = split << 8;
        let taken = if self.value >= big_split {
            self.range -= split;
            self.value -= big_split;
            1
        } else {
            self.range = split;
            0
        };
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;
            self.count += 1;
            if self.count == 8 {
                self.count = 0;
                self.value |= u32::from(self.byte());
            }
        }
        taken
    }

    fn flag(&mut self) -> bool {
        self.bit(128) != 0
    }

    /// An unsigned literal, most significant bit first.
    fn literal(&mut self, bits: u32) -> u32 {
        let mut value = 0;
        for _ in 0..bits {
            value = (value << 1) | self.bit(128);
        }
        value
    }

    /// A magnitude-then-sign literal, which is how every header delta is
    /// coded.
    fn signed(&mut self, bits: u32) -> i32 {
        let magnitude = i32::try_from(self.literal(bits)).unwrap_or(0);
        if self.flag() {
            -magnitude
        } else {
            magnitude
        }
    }

    /// A literal that is present only when a flag says so.
    fn optional_signed(&mut self, bits: u32) -> i32 {
        if self.flag() {
            self.signed(bits)
        } else {
            0
        }
    }

    /// Walk `tree` from node `start`, answering the leaf value reached.
    fn tree(&mut self, tree: &[i8], probs: &[u8], start: usize) -> usize {
        let mut node = start;
        for _ in 0..tree.len() {
            let probability = probs.get(node >> 1).copied().unwrap_or(128);
            let branch = node + usize::try_from(self.bit(probability)).unwrap_or(0);
            match tree.get(branch).copied().unwrap_or(0) {
                next if next > 0 => node = usize::try_from(next).unwrap_or(0),
                leaf => return usize::try_from(-i32::from(leaf)).unwrap_or(0),
            }
        }
        0
    }
}

/// Per-segment overrides of the quantiser index and loop-filter level.
struct Segmentation {
    enabled: bool,
    update_map: bool,
    absolute: bool,
    quant: [i32; SEGMENTS],
    filter: [i32; SEGMENTS],
    probs: [u8; 3],
}

impl Segmentation {
    const fn none() -> Self {
        Self {
            enabled: false,
            update_map: false,
            absolute: false,
            quant: [0; SEGMENTS],
            filter: [0; SEGMENTS],
            probs: [255; 3],
        }
    }

    fn read(&mut self, reader: &mut Bool<'_>) {
        self.enabled = true;
        self.update_map = reader.flag();
        if reader.flag() {
            self.absolute = reader.flag();
            for value in &mut self.quant {
                *value = reader.optional_signed(7);
            }
            for value in &mut self.filter {
                *value = reader.optional_signed(6);
            }
        }
        if self.update_map {
            for probability in &mut self.probs {
                *probability = if reader.flag() {
                    u8::try_from(reader.literal(8)).unwrap_or(255)
                } else {
                    255
                };
            }
        }
    }

    /// The quantiser index a segment uses, from the frame's own index.
    fn quant_index(&self, base: i32, segment: usize) -> i32 {
        let delta = self.quant.get(segment).copied().unwrap_or(0);
        let value = if self.absolute { delta } else { base + delta };
        value.clamp(0, 127)
    }

    /// The loop-filter level a segment uses, from the frame's own level.
    fn filter_level(&self, base: i32, segment: usize) -> i32 {
        if !self.enabled {
            return base;
        }
        let delta = self.filter.get(segment).copied().unwrap_or(0);
        if self.absolute {
            delta
        } else {
            base + delta
        }
    }
}

/// The loop-filter level adjustments a frame declares. Only the intra
/// reference delta and the subblock-mode delta apply to a keyframe.
struct FilterDeltas {
    enabled: bool,
    intra: i32,
    subblock: i32,
}

impl FilterDeltas {
    const fn none() -> Self {
        Self {
            enabled: false,
            intra: 0,
            subblock: 0,
        }
    }

    fn read(&mut self, reader: &mut Bool<'_>) {
        self.enabled = reader.flag();
        if !self.enabled || !reader.flag() {
            return;
        }
        let mut reference = [0i32; 4];
        for value in &mut reference {
            *value = reader.optional_signed(6);
        }
        let mut mode = [0i32; 4];
        for value in &mut mode {
            *value = reader.optional_signed(6);
        }
        self.intra = reference[0];
        self.subblock = mode[0];
    }
}

/// The dequantisation factors one segment's quantiser index yields.
#[derive(Copy, Clone, Default)]
struct Dequant {
    y_dc: i32,
    y_ac: i32,
    y2_dc: i32,
    y2_ac: i32,
    uv_dc: i32,
    uv_ac: i32,
}

/// The quantiser index and its per-plane deltas.
struct Quant {
    index: i32,
    y_dc: i32,
    y2_dc: i32,
    y2_ac: i32,
    uv_dc: i32,
    uv_ac: i32,
}

impl Quant {
    fn read(reader: &mut Bool<'_>) -> Self {
        Self {
            index: i32::try_from(reader.literal(7)).unwrap_or(0),
            y_dc: reader.optional_signed(4),
            y2_dc: reader.optional_signed(4),
            y2_ac: reader.optional_signed(4),
            uv_dc: reader.optional_signed(4),
            uv_ac: reader.optional_signed(4),
        }
    }

    /// The factors an index yields, with the scaling and clamping the format
    /// applies to the luma DC and chroma DC terms.
    fn factors(&self, index: i32) -> Dequant {
        let dc = |at: i32| {
            DC_QLOOKUP
                .get(usize::try_from(at.clamp(0, 127)).unwrap_or(0))
                .copied()
                .unwrap_or(4)
        };
        let ac = |at: i32| {
            AC_QLOOKUP
                .get(usize::try_from(at.clamp(0, 127)).unwrap_or(0))
                .copied()
                .unwrap_or(4)
        };
        Dequant {
            y_dc: dc(index + self.y_dc),
            y_ac: ac(index),
            y2_dc: dc(index + self.y2_dc) * 2,
            y2_ac: (ac(index + self.y2_ac) * 155 / 100).max(8),
            uv_dc: dc(index + self.uv_dc).min(132),
            uv_ac: ac(index + self.uv_ac),
        }
    }
}

/// One macroblock's coding decisions.
#[derive(Clone)]
struct Macroblock {
    segment: usize,
    skip: bool,
    /// The whole-macroblock luma mode, [`B_PRED`] when the subblocks carry
    /// their own.
    ymode: usize,
    uvmode: usize,
    bmodes: [usize; BLOCK_COEFFS],
}

/// Whether each of a macroblock's blocks decoded a non-zero coefficient,
/// which is the context its neighbours' token reads use.
#[derive(Copy, Clone, Default)]
struct Nonzero {
    y: [bool; 4],
    u: [bool; 2],
    v: [bool; 2],
    y2: bool,
}

/// One image plane, held with the one-pixel border prediction reads from and
/// four spare columns for the above-right samples a subblock mode needs.
struct Plane {
    samples: Vec<u8>,
    stride: usize,
    width: usize,
}

/// Spare columns each plane row carries past its width, so the rightmost
/// macroblock's above-right samples have somewhere to be.
const SPARE_COLUMNS: usize = 4;

impl Plane {
    fn new(width: usize, height: usize) -> Result<Self, DecodeError> {
        let stride = width
            .checked_add(1 + SPARE_COLUMNS)
            .ok_or(DecodeError::DimensionsOverflow)?;
        let len = stride
            .checked_mul(
                height
                    .checked_add(1)
                    .ok_or(DecodeError::DimensionsOverflow)?,
            )
            .ok_or(DecodeError::DimensionsOverflow)?;
        let mut samples = fallible::filled(len, 0u8).ok_or(DecodeError::OutOfMemory)?;
        // The row above the picture reads 127 and the column to its left
        // reads 129, which is what the format predicts from where a
        // neighbour does not exist. Their shared corner belongs to the row.
        if let Some(row) = samples.get_mut(..stride) {
            row.fill(ABOVE_ABSENT);
        }
        for y in 0..height {
            if let Some(sample) = samples.get_mut((y + 1) * stride) {
                *sample = LEFT_ABSENT;
            }
        }
        Ok(Self {
            samples,
            stride,
            width,
        })
    }

    /// The index of sample (`x`, `y`), where `-1` is the border.
    fn at(&self, x: isize, y: isize) -> usize {
        let column = usize::try_from(x + 1).unwrap_or(0);
        let row = usize::try_from(y + 1).unwrap_or(0);
        row * self.stride + column
    }

    fn get(&self, x: isize, y: isize) -> u8 {
        self.samples.get(self.at(x, y)).copied().unwrap_or(0)
    }

    fn set(&mut self, x: isize, y: isize, value: u8) {
        let index = self.at(x, y);
        if let Some(sample) = self.samples.get_mut(index) {
            *sample = value;
        }
    }

    /// Copy the rightmost sample of row `y` into the spare columns, so the
    /// rightmost macroblock of the row below predicts from it.
    fn extend_row(&mut self, y: isize) {
        let last = self.get(isize::try_from(self.width).unwrap_or(0) - 1, y);
        for spare in 0..SPARE_COLUMNS {
            let x = isize::try_from(self.width + spare).unwrap_or(0);
            self.set(x, y, last);
        }
    }
}

/// Everything a frame's header declares.
struct Header {
    width: u32,
    height: u32,
    simple_filter: bool,
    filter_level: i32,
    sharpness: u32,
    segmentation: Segmentation,
    deltas: FilterDeltas,
    quant: Quant,
    partitions: usize,
    probs: [[[[u8; NODES]; CONTEXTS]; BANDS]; PLANES],
    skip_probability: Option<u8>,
}

/// Read the uncompressed part of a keyframe header.
fn dimensions(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let head = bytes
        .get(..UNCOMPRESSED_HEADER)
        .ok_or(DecodeError::WebpLossyTruncated)?;
    let tag = u32::from(head[0]) | (u32::from(head[1]) << 8) | (u32::from(head[2]) << 16);
    if tag & 1 != 0 {
        return Err(DecodeError::WebpLossyInterframe);
    }
    if (tag >> 1) & 0x7 > 3 {
        return Err(DecodeError::WebpLossyUnsupportedProfile);
    }
    if head[3..6] != START_CODE {
        return Err(DecodeError::WebpLossyBadStartCode);
    }
    // The two scale fields above each dimension ask a display to stretch
    // the picture; the decoded picture is the size declared.
    let width = u32::from(u16::from_le_bytes([head[6], head[7]])) & 0x3FFF;
    let height = u32::from(u16::from_le_bytes([head[8], head[9]])) & 0x3FFF;
    if width == 0 || height == 0 {
        return Err(DecodeError::WebpLossyInvalidGeometry);
    }
    Ok((width, height))
}

/// How many bytes the header's first partition occupies.
fn first_partition(bytes: &[u8]) -> Result<usize, DecodeError> {
    let head = bytes.get(..3).ok_or(DecodeError::WebpLossyTruncated)?;
    let tag = u32::from(head[0]) | (u32::from(head[1]) << 8) | (u32::from(head[2]) << 16);
    Ok(usize::try_from(tag >> 5).unwrap_or(0))
}

/// Read the geometry a lossy bitstream declares, decoding no pixels.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    dimensions(bytes)
}

/// Decode a `VP8 ` keyframe into opaque straight-alpha RGBA8.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let (width, height) = dimensions(bytes)?;
    limits.check(width, height)?;
    let size = first_partition(bytes)?;
    let rest = bytes
        .get(UNCOMPRESSED_HEADER..)
        .ok_or(DecodeError::WebpLossyTruncated)?;
    let head = rest.get(..size).ok_or(DecodeError::WebpLossyTruncated)?;
    let mut reader = Bool::new(head);
    let header = read_header(&mut reader, width, height)?;
    let tokens = split_partitions(
        rest.get(size..).ok_or(DecodeError::WebpLossyTruncated)?,
        header.partitions,
    )?;
    let mut tokens = tokens;
    let mut frame = Frame::new(&header)?;
    frame.decode(&header, &mut reader, &mut tokens)?;
    if reader.exhausted() {
        return Err(DecodeError::WebpLossyTruncated);
    }
    frame.filter(&header);
    frame.to_rgba(width, height)
}

/// Read the compressed frame header.
fn read_header(reader: &mut Bool<'_>, width: u32, height: u32) -> Result<Header, DecodeError> {
    if reader.flag() {
        return Err(DecodeError::WebpLossyReservedColourSpace);
    }
    // Whether reconstruction needs clamping. Clamping regardless is correct
    // either way, so this is read and not acted on.
    let _clamping = reader.flag();
    let mut segmentation = Segmentation::none();
    if reader.flag() {
        segmentation.read(reader);
    }
    let simple_filter = reader.flag();
    let filter_level = i32::try_from(reader.literal(6)).unwrap_or(0);
    let sharpness = reader.literal(3);
    let mut deltas = FilterDeltas::none();
    deltas.read(reader);
    let partitions = 1usize << reader.literal(2).min(3);
    let quant = Quant::read(reader);
    // A keyframe's probabilities always start from the defaults, so whether
    // an update outlives this frame cannot matter to a single-frame decode.
    let _refresh_entropy = reader.flag();
    let mut probs = DEFAULT_COEFF_PROBS;
    for (plane, updates) in probs.iter_mut().zip(&COEFF_UPDATE_PROBS) {
        for (band, updates) in plane.iter_mut().zip(updates) {
            for (context, updates) in band.iter_mut().zip(updates) {
                for (probability, &update) in context.iter_mut().zip(updates) {
                    if reader.bit(update) != 0 {
                        *probability = u8::try_from(reader.literal(8)).unwrap_or(*probability);
                    }
                }
            }
        }
    }
    let skip_probability = reader
        .flag()
        .then(|| u8::try_from(reader.literal(8)).unwrap_or(0));
    Ok(Header {
        width,
        height,
        simple_filter,
        filter_level,
        sharpness,
        segmentation,
        deltas,
        quant,
        partitions,
        probs,
        skip_probability,
    })
}

/// Split the residual partitions from their declared sizes.
fn split_partitions(bytes: &[u8], count: usize) -> Result<Vec<Bool<'_>>, DecodeError> {
    let count = count.min(MAX_PARTITIONS);
    let table = (count - 1)
        .checked_mul(3)
        .ok_or(DecodeError::WebpLossyInvalidPartitions)?;
    let sizes = bytes
        .get(..table)
        .ok_or(DecodeError::WebpLossyInvalidPartitions)?;
    let mut body = bytes
        .get(table..)
        .ok_or(DecodeError::WebpLossyInvalidPartitions)?;
    let mut partitions = Vec::new();
    if !fallible::reserve(&mut partitions, count) {
        return Err(DecodeError::OutOfMemory);
    }
    for entry in sizes.as_chunks::<3>().0 {
        let size = usize::try_from(
            u32::from(entry[0]) | (u32::from(entry[1]) << 8) | (u32::from(entry[2]) << 16),
        )
        .unwrap_or(usize::MAX);
        let (partition, remainder) = body
            .split_at_checked(size)
            .ok_or(DecodeError::WebpLossyInvalidPartitions)?;
        partitions.push(Bool::new(partition));
        body = remainder;
    }
    partitions.push(Bool::new(body));
    Ok(partitions)
}

/// The planes a frame reconstructs into, and the contexts its walk carries.
struct Frame {
    luma: Plane,
    blue_chroma: Plane,
    red_chroma: Plane,
    columns: usize,
    rows: usize,
    /// Per-macroblock coding decisions, kept because the loop filter runs
    /// once the whole frame is reconstructed.
    blocks: Vec<Macroblock>,
    /// Whether each macroblock coded any non-zero coefficient, which decides
    /// whether its interior edges are filtered.
    coded: Vec<bool>,
    above: Vec<Nonzero>,
    left: Nonzero,
    /// The subblock-mode context of the row above, one entry per luma
    /// subblock column.
    above_modes: Vec<usize>,
    left_modes: [usize; 4],
}

impl Frame {
    fn new(header: &Header) -> Result<Self, DecodeError> {
        let columns = usize::try_from(header.width.div_ceil(16)).unwrap_or(0);
        let rows = usize::try_from(header.height.div_ceil(16)).unwrap_or(0);
        let count = columns
            .checked_mul(rows)
            .ok_or(DecodeError::DimensionsOverflow)?;
        let mut blocks = Vec::new();
        if !fallible::reserve(&mut blocks, count) {
            return Err(DecodeError::OutOfMemory);
        }
        Ok(Self {
            luma: Plane::new(columns * 16, rows * 16)?,
            blue_chroma: Plane::new(columns * 8, rows * 8)?,
            red_chroma: Plane::new(columns * 8, rows * 8)?,
            columns,
            rows,
            blocks,
            coded: fallible::filled(count, false).ok_or(DecodeError::OutOfMemory)?,
            above: fallible::filled(columns, Nonzero::default()).ok_or(DecodeError::OutOfMemory)?,
            left: Nonzero::default(),
            above_modes: fallible::filled(columns * 4, B_DC_PRED)
                .ok_or(DecodeError::OutOfMemory)?,
            left_modes: [B_DC_PRED; 4],
        })
    }

    /// Read every macroblock's modes and residuals and reconstruct it.
    fn decode(
        &mut self,
        header: &Header,
        modes: &mut Bool<'_>,
        tokens: &mut [Bool<'_>],
    ) -> Result<(), DecodeError> {
        let mut factors = [Dequant::default(); SEGMENTS];
        for (segment, factor) in factors.iter_mut().enumerate() {
            *factor = header
                .quant
                .factors(header.segmentation.quant_index(header.quant.index, segment));
        }
        for row in 0..self.rows {
            self.left = Nonzero::default();
            self.left_modes = [B_DC_PRED; 4];
            let partition = row % tokens.len().max(1);
            for column in 0..self.columns {
                let block = self.read_modes(header, modes, column);
                let reader = tokens
                    .get_mut(partition)
                    .ok_or(DecodeError::WebpLossyInvalidPartitions)?;
                let mut coeffs = [[0i16; BLOCK_COEFFS]; MB_BLOCKS];
                let coded = if block.skip {
                    self.skip_contexts(&block, column);
                    false
                } else {
                    self.read_residuals(header, reader, &block, column, &factors, &mut coeffs)
                };
                if reader.exhausted() {
                    return Err(DecodeError::WebpLossyTruncated);
                }
                self.reconstruct(&block, column, row, &coeffs);
                let index = row * self.columns + column;
                if let Some(slot) = self.coded.get_mut(index) {
                    *slot = coded;
                }
                self.blocks.push(block);
            }
            let last = isize::try_from(row * 16 + 15).unwrap_or(0);
            self.luma.extend_row(last);
            let chroma_last = isize::try_from(row * 8 + 7).unwrap_or(0);
            self.blue_chroma.extend_row(chroma_last);
            self.red_chroma.extend_row(chroma_last);
        }
        Ok(())
    }

    /// Read one macroblock's segment, skip flag, and intra modes.
    fn read_modes(&mut self, header: &Header, reader: &mut Bool<'_>, column: usize) -> Macroblock {
        let segment = if header.segmentation.enabled && header.segmentation.update_map {
            let probs = header.segmentation.probs;
            if reader.bit(probs[0]) != 0 {
                2 + usize::try_from(reader.bit(probs[2])).unwrap_or(0)
            } else {
                usize::try_from(reader.bit(probs[1])).unwrap_or(0)
            }
        } else {
            0
        };
        let skip = header
            .skip_probability
            .is_some_and(|probability| reader.bit(probability) != 0);
        let ymode = reader.tree(&KF_YMODE_TREE, &KF_YMODE_PROBS, 0);
        let mut bmodes = [B_DC_PRED; BLOCK_COEFFS];
        if ymode == B_PRED {
            for row in 0..4 {
                let mut left = self.left_modes[row];
                for column_in_mb in 0..4 {
                    let above = self
                        .above_modes
                        .get(column * 4 + column_in_mb)
                        .copied()
                        .unwrap_or(B_DC_PRED);
                    let probs = KF_BMODE_PROBS
                        .get(above)
                        .and_then(|row| row.get(left))
                        .unwrap_or(&KF_BMODE_PROBS[0][0]);
                    let mode = reader.tree(&BMODE_TREE, probs, 0);
                    bmodes[row * 4 + column_in_mb] = mode;
                    left = mode;
                    if let Some(slot) = self.above_modes.get_mut(column * 4 + column_in_mb) {
                        *slot = mode;
                    }
                }
                self.left_modes[row] = left;
            }
        } else {
            let implied = MODE_TO_BMODE.get(ymode).copied().unwrap_or(B_DC_PRED);
            bmodes = [implied; BLOCK_COEFFS];
            self.left_modes = [implied; 4];
            for offset in 0..4 {
                if let Some(slot) = self.above_modes.get_mut(column * 4 + offset) {
                    *slot = implied;
                }
            }
        }
        let uvmode = reader.tree(&UV_MODE_TREE, &KF_UV_MODE_PROBS, 0);
        Macroblock {
            segment,
            skip,
            ymode,
            uvmode,
            bmodes,
        }
    }

    /// Clear the token contexts a skipped macroblock leaves behind.
    ///
    /// A skipped macroblock coded no coefficients, so every block of it
    /// reads as zero to its neighbours — except the luma DC block of a
    /// subblock-predicted macroblock, which has none, so that context
    /// passes through untouched.
    fn skip_contexts(&mut self, block: &Macroblock, column: usize) {
        let y2 = if block.ymode == B_PRED {
            self.left.y2
        } else {
            false
        };
        self.left = Nonzero {
            y2,
            ..Nonzero::default()
        };
        if let Some(above) = self.above.get_mut(column) {
            *above = Nonzero {
                y2: if block.ymode == B_PRED {
                    above.y2
                } else {
                    false
                },
                ..Nonzero::default()
            };
        }
    }

    /// Read one macroblock's residual tokens, answering whether any block
    /// held a non-zero coefficient.
    fn read_residuals(
        &mut self,
        header: &Header,
        reader: &mut Bool<'_>,
        block: &Macroblock,
        column: usize,
        factors: &[Dequant; SEGMENTS],
        coeffs: &mut [[i16; BLOCK_COEFFS]; MB_BLOCKS],
    ) -> bool {
        let factor = factors.get(block.segment).copied().unwrap_or_default();
        let mut nonzero = [false; MB_BLOCKS];
        let has_y2 = block.ymode != B_PRED;
        let mut any = false;
        if has_y2 {
            let above = self.above.get(column).is_some_and(|above| above.y2);
            let context = usize::from(self.left.y2) + usize::from(above);
            let coded = read_block(
                reader,
                &header.probs,
                1,
                context,
                0,
                (factor.y2_dc, factor.y2_ac),
                &mut coeffs[Y2_BLOCK],
            );
            nonzero[Y2_BLOCK] = coded;
            any |= coded;
        }
        let plane = if has_y2 { 0 } else { 3 };
        let first = usize::from(has_y2);
        for index in 0..16 {
            let (row, column_in_mb) = (index / 4, index % 4);
            let left = if column_in_mb > 0 {
                nonzero[index - 1]
            } else {
                self.left.y[row]
            };
            let above = if row > 0 {
                nonzero[index - 4]
            } else {
                self.above
                    .get(column)
                    .is_some_and(|above| above.y[column_in_mb])
            };
            let coded = read_block(
                reader,
                &header.probs,
                plane,
                usize::from(left) + usize::from(above),
                first,
                (factor.y_dc, factor.y_ac),
                &mut coeffs[index],
            );
            nonzero[index] = coded;
            any |= coded;
        }
        for (base, is_blue) in [(16usize, true), (20usize, false)] {
            for index in 0..4 {
                let (row, column_in_mb) = (index / 2, index % 2);
                let at = base + index;
                let left = if column_in_mb > 0 {
                    nonzero[at - 1]
                } else if is_blue {
                    self.left.u[row]
                } else {
                    self.left.v[row]
                };
                let above = if row > 0 {
                    nonzero[at - 2]
                } else {
                    self.above.get(column).is_some_and(|above| {
                        if is_blue {
                            above.u[column_in_mb]
                        } else {
                            above.v[column_in_mb]
                        }
                    })
                };
                let coded = read_block(
                    reader,
                    &header.probs,
                    2,
                    usize::from(left) + usize::from(above),
                    0,
                    (factor.uv_dc, factor.uv_ac),
                    &mut coeffs[at],
                );
                nonzero[at] = coded;
                any |= coded;
            }
        }
        self.store_contexts(block, column, &nonzero);
        any
    }

    /// Carry a macroblock's non-zero flags into the contexts its right and
    /// lower neighbours read.
    fn store_contexts(&mut self, block: &Macroblock, column: usize, nonzero: &[bool; MB_BLOCKS]) {
        let mut left = Nonzero {
            y: [nonzero[3], nonzero[7], nonzero[11], nonzero[15]],
            u: [nonzero[17], nonzero[19]],
            v: [nonzero[21], nonzero[23]],
            y2: if block.ymode == B_PRED {
                self.left.y2
            } else {
                nonzero[Y2_BLOCK]
            },
        };
        core::mem::swap(&mut self.left, &mut left);
        if let Some(above) = self.above.get_mut(column) {
            *above = Nonzero {
                y: [nonzero[12], nonzero[13], nonzero[14], nonzero[15]],
                u: [nonzero[18], nonzero[19]],
                v: [nonzero[22], nonzero[23]],
                y2: if block.ymode == B_PRED {
                    above.y2
                } else {
                    nonzero[Y2_BLOCK]
                },
            };
        }
    }
}

/// Read one transform block's tokens, dequantise them into `coeffs`, and
/// answer whether any coefficient was non-zero.
fn read_block(
    reader: &mut Bool<'_>,
    probs: &[[[[u8; NODES]; CONTEXTS]; BANDS]; PLANES],
    plane: usize,
    context: usize,
    first: usize,
    (dc, ac): (i32, i32),
    coeffs: &mut [i16; BLOCK_COEFFS],
) -> bool {
    let mut context = context;
    let mut any = false;
    let mut previous_zero = false;
    for scan in first..BLOCK_COEFFS {
        let band = COEFF_BANDS.get(scan).copied().unwrap_or(0);
        let table = probs
            .get(plane)
            .and_then(|plane| plane.get(usize::from(band)))
            .and_then(|band| band.get(context))
            .unwrap_or(&probs[0][0][0]);
        let start = if previous_zero { TREE_AFTER_EOB } else { 0 };
        let token = reader.tree(&COEFF_TREE, table, start);
        if token == DCT_EOB {
            break;
        }
        if token == DCT_0 {
            context = 0;
            previous_zero = true;
            continue;
        }
        let magnitude = if token < DCT_CAT1 {
            i32::try_from(token).unwrap_or(0)
        } else {
            let category = token - DCT_CAT1;
            let extra = CATEGORY_PROBS
                .get(category)
                .map_or(0, |bits| extra_bits(reader, bits));
            CATEGORY_BASES.get(category).copied().unwrap_or(0) + extra
        };
        context = if magnitude == 1 { 1 } else { 2 };
        previous_zero = false;
        any = true;
        let signed = if reader.flag() { -magnitude } else { magnitude };
        let factor = if scan == 0 { dc } else { ac };
        // The format stores dequantised coefficients in sixteen bits, which
        // a conforming stream's own ranges stay inside; a product that does
        // not saturates rather than wrapping a large positive into a large
        // negative.
        let position = ZIGZAG.get(scan).copied().unwrap_or(0);
        if let Some(slot) = coeffs.get_mut(usize::from(position)) {
            *slot = i16::try_from(signed.saturating_mul(factor)).unwrap_or(if signed < 0 {
                i16::MIN
            } else {
                i16::MAX
            });
        }
    }
    any
}

/// Read a large coefficient's extra bits, most significant first.
fn extra_bits(reader: &mut Bool<'_>, probs: &[u8]) -> i32 {
    let mut value = 0i32;
    for &probability in probs {
        value = 2 * value + i32::try_from(reader.bit(probability)).unwrap_or(0);
    }
    value
}

/// Clamp a filter temporary into signed eight bits, as the filters do.
fn clamp_s8(value: i32) -> i32 {
    value.clamp(-128, 127)
}

/// Clamp a reconstructed sample into pixel range.
fn clamp_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(0)
}

/// The weighted average of three adjacent samples, centred on the second.
fn avg3(x: u8, y: u8, z: u8) -> u8 {
    let sum = i32::from(x) + 2 * i32::from(y) + i32::from(z) + 2;
    clamp_u8(sum >> 2)
}

/// The average of two adjacent samples.
fn avg2(x: u8, y: u8) -> u8 {
    clamp_u8((i32::from(x) + i32::from(y) + 1) >> 1)
}

/// The inverse Walsh-Hadamard transform of a macroblock's luma DC block,
/// whose outputs become the sixteen luma blocks' DC coefficients.
fn inverse_walsh(input: &[i16; BLOCK_COEFFS]) -> [i16; BLOCK_COEFFS] {
    let mut middle = [0i32; BLOCK_COEFFS];
    for column in 0..4 {
        let at = |row: usize| i32::from(input[row * 4 + column]);
        let a = at(0) + at(3);
        let b = at(1) + at(2);
        let c = at(1) - at(2);
        let d = at(0) - at(3);
        middle[column] = a + b;
        middle[4 + column] = c + d;
        middle[8 + column] = a - b;
        middle[12 + column] = d - c;
    }
    let mut output = [0i16; BLOCK_COEFFS];
    for row in 0..4 {
        let at = |column: usize| middle[row * 4 + column];
        let a = at(0) + at(3);
        let b = at(1) + at(2);
        let c = at(1) - at(2);
        let d = at(0) - at(3);
        let write = |value: i32| i16::try_from((value + 3) >> 3).unwrap_or(0);
        output[row * 4] = write(a + b);
        output[row * 4 + 1] = write(c + d);
        output[row * 4 + 2] = write(a - b);
        output[row * 4 + 3] = write(d - c);
    }
    output
}

/// The inverse DCT of one transform block, answering its residual samples.
fn inverse_dct(input: &[i16; BLOCK_COEFFS]) -> [i32; BLOCK_COEFFS] {
    let mut middle = [0i32; BLOCK_COEFFS];
    for column in 0..4 {
        let at = |row: usize| i32::from(input[row * 4 + column]);
        let a = at(0) + at(2);
        let b = at(0) - at(2);
        let c = ((at(1) * SIN_PI8_SQRT2) >> 16) - (at(3) + ((at(3) * COS_PI8_SQRT2_MINUS1) >> 16));
        let d = (at(1) + ((at(1) * COS_PI8_SQRT2_MINUS1) >> 16)) + ((at(3) * SIN_PI8_SQRT2) >> 16);
        middle[column] = a + d;
        middle[12 + column] = a - d;
        middle[4 + column] = b + c;
        middle[8 + column] = b - c;
    }
    let mut output = [0i32; BLOCK_COEFFS];
    for row in 0..4 {
        let at = |column: usize| middle[row * 4 + column];
        let a = at(0) + at(2);
        let b = at(0) - at(2);
        let c = ((at(1) * SIN_PI8_SQRT2) >> 16) - (at(3) + ((at(3) * COS_PI8_SQRT2_MINUS1) >> 16));
        let d = (at(1) + ((at(1) * COS_PI8_SQRT2_MINUS1) >> 16)) + ((at(3) * SIN_PI8_SQRT2) >> 16);
        output[row * 4] = (a + d + 4) >> 3;
        output[row * 4 + 3] = (a - d + 4) >> 3;
        output[row * 4 + 1] = (b + c + 4) >> 3;
        output[row * 4 + 2] = (b - c + 4) >> 3;
    }
    output
}

/// Add one transform block's residual to the samples predicted for it.
///
/// A block whose only non-zero coefficient is its first has a constant
/// residual, which the full transform would reach the long way round.
fn add_residual(plane: &mut Plane, x0: isize, y0: isize, coeffs: &[i16; BLOCK_COEFFS]) {
    let ac_present = coeffs.iter().skip(1).any(|&value| value != 0);
    if !ac_present {
        if coeffs[0] == 0 {
            return;
        }
        let flat = (i32::from(coeffs[0]) + 4) >> 3;
        for row in 0..4 {
            for column in 0..4 {
                let x = x0 + isize::try_from(column).unwrap_or(0);
                let y = y0 + isize::try_from(row).unwrap_or(0);
                let value = i32::from(plane.get(x, y)) + flat;
                plane.set(x, y, clamp_u8(value));
            }
        }
        return;
    }
    let residual = inverse_dct(coeffs);
    for (row, samples) in residual.as_chunks::<4>().0.iter().enumerate() {
        for (column, &offset) in samples.iter().enumerate() {
            let x = x0 + isize::try_from(column).unwrap_or(0);
            let y = y0 + isize::try_from(row).unwrap_or(0);
            let value = i32::from(plane.get(x, y)) + offset;
            plane.set(x, y, clamp_u8(value));
        }
    }
}

/// Predict a whole 16x16 or 8x8 block from its above row and left column.
fn predict_block(plane: &mut Plane, x0: isize, y0: isize, size: isize, mode: usize) {
    let has_above = y0 > 0;
    let has_left = x0 > 0;
    match mode {
        V_PRED => {
            for column in 0..size {
                let value = plane.get(x0 + column, y0 - 1);
                for row in 0..size {
                    plane.set(x0 + column, y0 + row, value);
                }
            }
        }
        H_PRED => {
            for row in 0..size {
                let value = plane.get(x0 - 1, y0 + row);
                for column in 0..size {
                    plane.set(x0 + column, y0 + row, value);
                }
            }
        }
        TM_PRED => {
            let corner = i32::from(plane.get(x0 - 1, y0 - 1));
            for row in 0..size {
                let left = i32::from(plane.get(x0 - 1, y0 + row));
                for column in 0..size {
                    let above = i32::from(plane.get(x0 + column, y0 - 1));
                    plane.set(x0 + column, y0 + row, clamp_u8(left + above - corner));
                }
            }
        }
        // Every mode but the three above averages what is available, and a
        // block with neither neighbour has nothing to average.
        _ => {
            let mut sum = 0i32;
            let mut counted = 0i32;
            if has_above {
                for column in 0..size {
                    sum += i32::from(plane.get(x0 + column, y0 - 1));
                }
                counted += i32::try_from(size).unwrap_or(0);
            }
            if has_left {
                for row in 0..size {
                    sum += i32::from(plane.get(x0 - 1, y0 + row));
                }
                counted += i32::try_from(size).unwrap_or(0);
            }
            let value = if counted == 0 {
                128
            } else {
                let shift = counted.trailing_zeros();
                (sum + (1 << (shift - 1))) >> shift
            };
            let value = clamp_u8(value);
            for row in 0..size {
                for column in 0..size {
                    plane.set(x0 + column, y0 + row, value);
                }
            }
        }
    }
}

/// Predict one 4x4 luma subblock.
///
/// `above` holds the eight samples above the subblock — its own four
/// followed by the four above and to the right — and `corner` the sample
/// above and to the left of both.
fn predict_subblock(
    plane: &mut Plane,
    x0: isize,
    y0: isize,
    mode: usize,
    above: [u8; 8],
    corner: u8,
) {
    let left = [
        plane.get(x0 - 1, y0),
        plane.get(x0 - 1, y0 + 1),
        plane.get(x0 - 1, y0 + 2),
        plane.get(x0 - 1, y0 + 3),
    ];
    // The nine already-reconstructed edge samples the diagonal modes run
    // along, from the bottom of the left column round to the right of the
    // above row.
    let edge = [
        left[3], left[2], left[1], left[0], corner, above[0], above[1], above[2], above[3],
    ];
    let mut block = [[0u8; 4]; 4];
    let smooth3_above = |at: usize| avg3(above[at - 1], above[at], above[at + 1]);
    match mode {
        B_TM_PRED => {
            for row in 0..4 {
                for column in 0..4 {
                    block[row][column] = clamp_u8(
                        i32::from(left[row]) + i32::from(above[column]) - i32::from(corner),
                    );
                }
            }
        }
        B_VE_PRED => {
            let smoothed = [
                avg3(corner, above[0], above[1]),
                smooth3_above(1),
                smooth3_above(2),
                smooth3_above(3),
            ];
            block.fill(smoothed);
        }
        B_HE_PRED => {
            let smoothed = [
                avg3(corner, left[0], left[1]),
                avg3(left[0], left[1], left[2]),
                avg3(left[1], left[2], left[3]),
                avg3(left[2], left[3], left[3]),
            ];
            for (row, value) in block.iter_mut().zip(smoothed) {
                *row = [value; 4];
            }
        }
        // The six modes that subdivide the block into diagonal lines are
        // the bulk of the work and have no full-block analogue.
        B_LD_PRED | B_RD_PRED | B_VR_PRED | B_VL_PRED | B_HD_PRED | B_HU_PRED => {
            predict_diagonal(&mut block, mode, above, &edge, left);
        }
        // The remaining mode averages the eight samples around the
        // subblock's upper-left corner.
        _ => {
            let mut sum = 4i32;
            for (&up, &side) in above.iter().take(4).zip(&left) {
                sum += i32::from(up) + i32::from(side);
            }
            let value = clamp_u8(sum >> 3);
            block = [[value; 4]; 4];
        }
    }
    for (row, samples) in block.iter().enumerate() {
        for (column, &value) in samples.iter().enumerate() {
            plane.set(
                x0 + isize::try_from(column).unwrap_or(0),
                y0 + isize::try_from(row).unwrap_or(0),
                value,
            );
        }
    }
}

/// Predict one 4x4 luma subblock under a diagonal mode.
///
/// Each mode assigns every pixel of a diagonal line the same value: a
/// smoothed or half-step version of an already-reconstructed edge sample
/// lying on that line. `edge` holds those samples from the bottom of the
/// left column round to the right of the above row.
fn predict_diagonal(
    block: &mut [[u8; 4]; 4],
    mode: usize,
    above: [u8; 8],
    edge: &[u8; 9],
    left: [u8; 4],
) {
    let smooth3 =
        |at: usize, samples: &[u8; 9]| avg3(samples[at - 1], samples[at], samples[at + 1]);
    let smooth3_above = |at: usize| avg3(above[at - 1], above[at], above[at + 1]);
    match mode {
        B_LD_PRED => {
            block[0][0] = smooth3_above(1);
            block[0][1] = smooth3_above(2);
            block[1][0] = block[0][1];
            block[0][2] = smooth3_above(3);
            block[1][1] = block[0][2];
            block[2][0] = block[0][2];
            block[0][3] = smooth3_above(4);
            block[1][2] = block[0][3];
            block[2][1] = block[0][3];
            block[3][0] = block[0][3];
            block[1][3] = smooth3_above(5);
            block[2][2] = block[1][3];
            block[3][1] = block[1][3];
            block[2][3] = smooth3_above(6);
            block[3][2] = block[2][3];
            block[3][3] = avg3(above[6], above[7], above[7]);
        }
        B_RD_PRED => {
            block[3][0] = smooth3(1, edge);
            block[3][1] = smooth3(2, edge);
            block[2][0] = block[3][1];
            block[3][2] = smooth3(3, edge);
            block[2][1] = block[3][2];
            block[1][0] = block[3][2];
            block[3][3] = smooth3(4, edge);
            block[2][2] = block[3][3];
            block[1][1] = block[3][3];
            block[0][0] = block[3][3];
            block[2][3] = smooth3(5, edge);
            block[1][2] = block[2][3];
            block[0][1] = block[2][3];
            block[1][3] = smooth3(6, edge);
            block[0][2] = block[1][3];
            block[0][3] = smooth3(7, edge);
        }
        // The remaining four use lines of slope two and one half, which
        // often need a sample synthesised midway between two real ones.
        _ => predict_shallow_diagonal(block, mode, above, edge, left),
    }
}

/// Predict one 4x4 luma subblock under a shallow diagonal mode, whose lines
/// run at roughly 27 degrees from an axis.
fn predict_shallow_diagonal(
    block: &mut [[u8; 4]; 4],
    mode: usize,
    above: [u8; 8],
    edge: &[u8; 9],
    left: [u8; 4],
) {
    let smooth3 =
        |at: usize, samples: &[u8; 9]| avg3(samples[at - 1], samples[at], samples[at + 1]);
    let smooth3_above = |at: usize| avg3(above[at - 1], above[at], above[at + 1]);
    let mean2 = |at: usize, samples: &[u8; 9]| avg2(samples[at], samples[at + 1]);
    match mode {
        B_VR_PRED => {
            block[3][0] = smooth3(2, edge);
            block[2][0] = smooth3(3, edge);
            block[3][1] = smooth3(4, edge);
            block[1][0] = block[3][1];
            block[2][1] = mean2(4, edge);
            block[0][0] = block[2][1];
            block[3][2] = smooth3(5, edge);
            block[1][1] = block[3][2];
            block[2][2] = mean2(5, edge);
            block[0][1] = block[2][2];
            block[3][3] = smooth3(6, edge);
            block[1][2] = block[3][3];
            block[2][3] = mean2(6, edge);
            block[0][2] = block[2][3];
            block[1][3] = smooth3(7, edge);
            block[0][3] = mean2(7, edge);
        }
        B_VL_PRED => {
            block[0][0] = avg2(above[0], above[1]);
            block[1][0] = smooth3_above(1);
            block[2][0] = avg2(above[1], above[2]);
            block[0][1] = block[2][0];
            block[1][1] = smooth3_above(2);
            block[3][0] = block[1][1];
            block[2][1] = avg2(above[2], above[3]);
            block[0][2] = block[2][1];
            block[3][1] = smooth3_above(3);
            block[1][2] = block[3][1];
            block[2][2] = avg2(above[3], above[4]);
            block[0][3] = block[2][2];
            block[3][2] = smooth3_above(4);
            block[1][3] = block[3][2];
            // The last two do not continue the pattern: no reconstructed
            // sample lies on their diagonals.
            block[2][3] = smooth3_above(5);
            block[3][3] = smooth3_above(6);
        }
        B_HD_PRED => {
            block[3][0] = avg2(edge[0], edge[1]);
            block[3][1] = smooth3(1, edge);
            block[2][0] = mean2(1, edge);
            block[3][2] = block[2][0];
            block[2][1] = smooth3(2, edge);
            block[3][3] = block[2][1];
            block[2][2] = mean2(2, edge);
            block[1][0] = block[2][2];
            block[2][3] = smooth3(3, edge);
            block[1][1] = block[2][3];
            block[1][2] = mean2(3, edge);
            block[0][0] = block[1][2];
            block[1][3] = smooth3(4, edge);
            block[0][1] = block[1][3];
            block[0][2] = smooth3(5, edge);
            block[0][3] = smooth3(6, edge);
        }
        B_HU_PRED => {
            block[0][0] = avg2(left[0], left[1]);
            block[0][1] = avg3(left[0], left[1], left[2]);
            block[0][2] = avg2(left[1], left[2]);
            block[1][0] = block[0][2];
            block[0][3] = avg3(left[1], left[2], left[3]);
            block[1][1] = block[0][3];
            block[1][2] = avg2(left[2], left[3]);
            block[2][0] = block[1][2];
            block[1][3] = avg3(left[2], left[3], left[3]);
            block[2][1] = block[1][3];
            block[2][2] = left[3];
            block[2][3] = left[3];
            block[3] = [left[3]; 4];
        }
        _ => {}
    }
}

impl Frame {
    /// Predict one macroblock and add its residual.
    fn reconstruct(
        &mut self,
        block: &Macroblock,
        column: usize,
        row: usize,
        coeffs: &[[i16; BLOCK_COEFFS]; MB_BLOCKS],
    ) {
        let mut luma = *coeffs;
        if block.ymode != B_PRED {
            let dc = inverse_walsh(&coeffs[Y2_BLOCK]);
            for (index, value) in dc.iter().enumerate() {
                luma[index][0] = *value;
            }
        }
        let x0 = isize::try_from(column * 16).unwrap_or(0);
        let y0 = isize::try_from(row * 16).unwrap_or(0);
        if block.ymode == B_PRED {
            // The rightmost subblocks of a macroblock predict from the four
            // samples above and to the right of the macroblock, because
            // their own upper-right neighbours are not reconstructed yet.
            let mut edge = [ABOVE_ABSENT; 4];
            for (offset, sample) in edge.iter_mut().enumerate() {
                *sample = self
                    .luma
                    .get(x0 + 16 + isize::try_from(offset).unwrap_or(0), y0 - 1);
            }
            for (index, residual) in luma.iter().enumerate().take(16) {
                let (sub_row, sub_column) = (index / 4, index % 4);
                let x = x0 + isize::try_from(sub_column * 4).unwrap_or(0);
                let y = y0 + isize::try_from(sub_row * 4).unwrap_or(0);
                let mut above = [0u8; 8];
                for (offset, sample) in above.iter_mut().enumerate().take(4) {
                    *sample = self
                        .luma
                        .get(x + isize::try_from(offset).unwrap_or(0), y - 1);
                }
                if sub_column == 3 {
                    above[4..8].copy_from_slice(&edge);
                } else {
                    for (offset, sample) in above.iter_mut().enumerate().skip(4) {
                        *sample = self
                            .luma
                            .get(x + isize::try_from(offset).unwrap_or(0), y - 1);
                    }
                }
                let corner = self.luma.get(x - 1, y - 1);
                predict_subblock(&mut self.luma, x, y, block.bmodes[index], above, corner);
                add_residual(&mut self.luma, x, y, residual);
            }
        } else {
            predict_block(&mut self.luma, x0, y0, 16, block.ymode);
            for (index, residual) in luma.iter().enumerate().take(16) {
                let x = x0 + isize::try_from((index % 4) * 4).unwrap_or(0);
                let y = y0 + isize::try_from((index / 4) * 4).unwrap_or(0);
                add_residual(&mut self.luma, x, y, residual);
            }
        }
        let cx = isize::try_from(column * 8).unwrap_or(0);
        let cy = isize::try_from(row * 8).unwrap_or(0);
        predict_block(&mut self.blue_chroma, cx, cy, 8, block.uvmode);
        predict_block(&mut self.red_chroma, cx, cy, 8, block.uvmode);
        for index in 0..4 {
            let x = cx + isize::try_from((index % 2) * 4).unwrap_or(0);
            let y = cy + isize::try_from((index / 2) * 4).unwrap_or(0);
            add_residual(&mut self.blue_chroma, x, y, &luma[16 + index]);
            add_residual(&mut self.red_chroma, x, y, &luma[20 + index]);
        }
    }

    /// Apply the loop filter over the whole reconstructed frame.
    fn filter(&mut self, header: &Header) {
        if header.filter_level == 0 {
            return;
        }
        for row in 0..self.rows {
            for column in 0..self.columns {
                let index = row * self.columns + column;
                let Some(block) = self.blocks.get(index) else {
                    continue;
                };
                let subblocks = block.ymode == B_PRED;
                let coded = self.coded.get(index).copied().unwrap_or(false);
                let inner = subblocks || coded;
                let mut level = header
                    .segmentation
                    .filter_level(header.filter_level, block.segment);
                if header.deltas.enabled {
                    level += header.deltas.intra;
                    if subblocks {
                        level += header.deltas.subblock;
                    }
                }
                let level = level.clamp(0, 63);
                if level == 0 {
                    continue;
                }
                let mut interior = level;
                if header.sharpness > 0 {
                    interior >>= if header.sharpness > 4 { 2 } else { 1 };
                    interior = interior.min(9 - i32::try_from(header.sharpness).unwrap_or(0));
                }
                let interior = interior.max(1);
                let edge = 2 * (level + 2) + interior;
                let inside = 2 * level + interior;
                let hev = i32::from(level >= 40) + i32::from(level >= 15);
                let strength = Strength {
                    edge,
                    inside,
                    interior,
                    hev,
                    inner,
                };
                if header.simple_filter {
                    self.filter_simple(column, row, &strength);
                } else {
                    self.filter_normal(column, row, &strength);
                }
            }
        }
    }

    /// The simple filter, which covers luma edges only.
    fn filter_simple(&mut self, column: usize, row: usize, strength: &Strength) {
        let x0 = isize::try_from(column * 16).unwrap_or(0);
        let y0 = isize::try_from(row * 16).unwrap_or(0);
        if column > 0 {
            simple_edge(&mut self.luma, x0, y0, 16, true, strength.edge);
        }
        if strength.inner {
            for offset in [4, 8, 12] {
                simple_edge(&mut self.luma, x0 + offset, y0, 16, true, strength.inside);
            }
        }
        if row > 0 {
            simple_edge(&mut self.luma, x0, y0, 16, false, strength.edge);
        }
        if strength.inner {
            for offset in [4, 8, 12] {
                simple_edge(&mut self.luma, x0, y0 + offset, 16, false, strength.inside);
            }
        }
    }

    /// The normal filter, which covers luma and both chroma planes.
    fn filter_normal(&mut self, column: usize, row: usize, strength: &Strength) {
        let x0 = isize::try_from(column * 16).unwrap_or(0);
        let y0 = isize::try_from(row * 16).unwrap_or(0);
        let cx = isize::try_from(column * 8).unwrap_or(0);
        let cy = isize::try_from(row * 8).unwrap_or(0);
        if column > 0 {
            macroblock_edge(&mut self.luma, x0, y0, 16, true, strength);
            macroblock_edge(&mut self.blue_chroma, cx, cy, 8, true, strength);
            macroblock_edge(&mut self.red_chroma, cx, cy, 8, true, strength);
        }
        if strength.inner {
            for offset in [4, 8, 12] {
                subblock_edge(&mut self.luma, x0 + offset, y0, 16, true, strength);
            }
            subblock_edge(&mut self.blue_chroma, cx + 4, cy, 8, true, strength);
            subblock_edge(&mut self.red_chroma, cx + 4, cy, 8, true, strength);
        }
        if row > 0 {
            macroblock_edge(&mut self.luma, x0, y0, 16, false, strength);
            macroblock_edge(&mut self.blue_chroma, cx, cy, 8, false, strength);
            macroblock_edge(&mut self.red_chroma, cx, cy, 8, false, strength);
        }
        if strength.inner {
            for offset in [4, 8, 12] {
                subblock_edge(&mut self.luma, x0, y0 + offset, 16, false, strength);
            }
            subblock_edge(&mut self.blue_chroma, cx, cy + 4, 8, false, strength);
            subblock_edge(&mut self.red_chroma, cx, cy + 4, 8, false, strength);
        }
    }

    /// Convert the reconstructed planes to straight-alpha RGBA8, cropping
    /// the padding the macroblock grid added.
    fn to_rgba(&self, width: u32, height: u32) -> Result<RasterImage, DecodeError> {
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|count| count.checked_mul(RGBA_BYTES as u64))
            .ok_or(DecodeError::DimensionsOverflow)?;
        let len = usize::try_from(pixels).map_err(|_| DecodeError::DimensionsOverflow)?;
        let mut rgba = fallible::filled(len, 0u8).ok_or(DecodeError::OutOfMemory)?;
        let mut at = 0usize;
        for y in 0..isize::try_from(height).unwrap_or(0) {
            for x in 0..isize::try_from(width).unwrap_or(0) {
                let luma = (i32::from(self.luma.get(x, y)) * 19077) >> 8;
                let blue = i32::from(self.blue_chroma.get(x / 2, y / 2));
                let red = i32::from(self.red_chroma.get(x / 2, y / 2));
                let Some(pixel) = rgba.get_mut(at..at + RGBA_BYTES) else {
                    break;
                };
                pixel[0] = to_channel(luma + ((red * 26149) >> 8) - 14234);
                pixel[1] = to_channel(luma - ((blue * 6419) >> 8) - ((red * 13320) >> 8) + 8708);
                pixel[2] = to_channel(luma + ((blue * 33050) >> 8) - 17685);
                pixel[3] = u8::MAX;
                at += RGBA_BYTES;
            }
        }
        Ok(RasterImage::from_parts(width, height, rgba))
    }
}

/// One channel of the fixed-point colour conversion, rounded and clamped.
fn to_channel(value: i32) -> u8 {
    clamp_u8(value >> 6)
}

/// The thresholds one macroblock's edges are filtered under.
struct Strength {
    edge: i32,
    inside: i32,
    interior: i32,
    hev: i32,
    inner: bool,
}

/// The eight sample indices straddling one edge, from four before it to four
/// after.
fn straddle(plane: &Plane, x: isize, y: isize, vertical: bool) -> [usize; 8] {
    let step = if vertical {
        1isize
    } else {
        isize::try_from(plane.stride).unwrap_or(1)
    };
    let centre = isize::try_from(plane.at(x, y)).unwrap_or(0);
    let mut indices = [0usize; 8];
    for (offset, index) in indices.iter_mut().enumerate() {
        let at = centre + (isize::try_from(offset).unwrap_or(0) - 4) * step;
        *index = usize::try_from(at).unwrap_or(0);
    }
    indices
}

/// The eight samples straddling one edge, converted to signed.
fn read_straddle(plane: &Plane, indices: &[usize; 8]) -> [i32; 8] {
    let mut values = [0i32; 8];
    for (value, &index) in values.iter_mut().zip(indices) {
        *value = i32::from(plane.samples.get(index).copied().unwrap_or(128)) - 128;
    }
    values
}

/// Store one signed sample back into pixel range.
fn write_sample(plane: &mut Plane, index: usize, value: i32) {
    if let Some(sample) = plane.samples.get_mut(index) {
        *sample = clamp_u8(clamp_s8(value) + 128);
    }
}

/// Whether an edge's four straddling samples are close enough to filter and
/// its interior differences small enough (RFC 6386 §15.3).
fn filterable(values: &[i32; 8], interior: i32, edge: i32) -> bool {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = *values;
    (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 <= edge
        && (p3 - p2).abs() <= interior
        && (p2 - p1).abs() <= interior
        && (p1 - p0).abs() <= interior
        && (q3 - q2).abs() <= interior
        && (q2 - q1).abs() <= interior
        && (q1 - q0).abs() <= interior
}

/// Whether the samples either side of the edge differ sharply enough that
/// the wider filters would blur a real feature.
fn high_variance(values: &[i32; 8], threshold: i32) -> bool {
    (values[2] - values[3]).abs() > threshold || (values[5] - values[4]).abs() > threshold
}

/// The adjustment both filters share, applied to the two edge samples.
fn common_adjust(
    plane: &mut Plane,
    indices: &[usize; 8],
    values: &[i32; 8],
    outer_taps: bool,
) -> i32 {
    let (p1, p0, q0, q1) = (values[2], values[3], values[4], values[5]);
    let base = clamp_s8(if outer_taps { clamp_s8(p1 - q1) } else { 0 } + 3 * (q0 - p0));
    let toward = clamp_s8(base + 3) >> 3;
    let away = clamp_s8(base + 4) >> 3;
    write_sample(plane, indices[4], q0 - away);
    write_sample(plane, indices[3], p0 + toward);
    away
}

/// The simple filter over one edge's segments.
fn simple_edge(plane: &mut Plane, x: isize, y: isize, span: isize, vertical: bool, limit: i32) {
    for along in 0..span {
        let (sx, sy) = if vertical {
            (x, y + along)
        } else {
            (x + along, y)
        };
        let indices = straddle(plane, sx, sy, vertical);
        let values = read_straddle(plane, &indices);
        let (p1, p0, q0, q1) = (values[2], values[3], values[4], values[5]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 <= limit {
            common_adjust(plane, &indices, &values, true);
        }
    }
}

/// The normal filter over one subblock edge's segments.
fn subblock_edge(
    plane: &mut Plane,
    x: isize,
    y: isize,
    span: isize,
    vertical: bool,
    strength: &Strength,
) {
    for along in 0..span {
        let (sx, sy) = if vertical {
            (x, y + along)
        } else {
            (x + along, y)
        };
        let indices = straddle(plane, sx, sy, vertical);
        let values = read_straddle(plane, &indices);
        if !filterable(&values, strength.interior, strength.inside) {
            continue;
        }
        let sharp = high_variance(&values, strength.hev);
        let adjust = (common_adjust(plane, &indices, &values, sharp) + 1) >> 1;
        if !sharp {
            write_sample(plane, indices[5], values[5] - adjust);
            write_sample(plane, indices[2], values[2] + adjust);
        }
    }
}

/// The normal filter over one macroblock edge's segments, which reaches
/// three samples either side.
fn macroblock_edge(
    plane: &mut Plane,
    x: isize,
    y: isize,
    span: isize,
    vertical: bool,
    strength: &Strength,
) {
    for along in 0..span {
        let (sx, sy) = if vertical {
            (x, y + along)
        } else {
            (x + along, y)
        };
        let indices = straddle(plane, sx, sy, vertical);
        let values = read_straddle(plane, &indices);
        if !filterable(&values, strength.interior, strength.edge) {
            continue;
        }
        if high_variance(&values, strength.hev) {
            common_adjust(plane, &indices, &values, true);
            continue;
        }
        let (p2, p1, p0, q0, q1, q2) = (
            values[1], values[2], values[3], values[4], values[5], values[6],
        );
        let width = clamp_s8(clamp_s8(p1 - q1) + 3 * (q0 - p0));
        let near = clamp_s8((27 * width + 63) >> 7);
        write_sample(plane, indices[4], q0 - near);
        write_sample(plane, indices[3], p0 + near);
        let middle = clamp_s8((18 * width + 63) >> 7);
        write_sample(plane, indices[5], q1 - middle);
        write_sample(plane, indices[2], p1 + middle);
        let far = clamp_s8((9 * width + 63) >> 7);
        write_sample(plane, indices[6], q2 - far);
        write_sample(plane, indices[1], p2 + far);
    }
}

#[cfg(test)]
#[path = "vp8_fixture.rs"]
pub(crate) mod fixture;

#[cfg(test)]
#[path = "vp8_tests.rs"]
mod tests;
