//! GF(2^8) arithmetic for the RAID6 double-parity Q syndrome.
//!
//! RAID6 protects a stripe with two independent syndromes over the finite
//! field GF(2^8): the P syndrome is the bytewise XOR of the data chunks (the
//! same parity RAID5 uses), and the Q syndrome is a Reed-Solomon syndrome
//! `Q = Σ gᵏ · Dₖ` where `g` is the field generator `0x02` and `Dₖ` is the
//! `k`-th data chunk. Because P and Q are linear and independent, any two lost
//! chunks in a stripe can be solved for from the survivors (`AGENTS.md` §26.5
//! two-fault redundancy). This is the standard Linux-md / Anvin RAID6 scheme.
//!
//! The field uses the primitive polynomial `x^8 + x^4 + x^3 + x^2 + 1`
//! (`0x11d`), identical to the Linux RAID6 implementation, so an array is
//! wire-compatible with that well-understood layout. All arithmetic is a pure
//! function of its byte inputs — no allocation, no `unsafe`, and constant with
//! respect to control flow over the field — so it is fully host-testable.

/// The field generator used for the Q syndrome (`g = {02}`), matching the
/// Linux RAID6 convention.
pub(crate) const GENERATOR: u8 = 0x02;

/// Multiply two field elements in GF(2^8) with the reducing polynomial
/// `0x11d`. Russian-peasant multiplication, branch-uniform over the eight
/// bit positions.
#[must_use]
pub(crate) const fn mul(a: u8, b: u8) -> u8 {
    let mut a = a;
    let mut b = b;
    let mut product: u8 = 0;
    let mut i = 0;
    while i < 8 {
        // Add `a` into the product when the low bit of `b` is set. The mask is
        // `0xff` for a set bit and `0x00` otherwise, so the add is uniform.
        let add_mask = 0u8.wrapping_sub(b & 1);
        product ^= a & add_mask;
        // Multiply `a` by x, reducing modulo the field polynomial when it
        // overflows bit 7 — again through a uniform mask, no branch.
        let carry = a & 0x80;
        let reduce_mask = 0u8.wrapping_sub(carry >> 7);
        a = (a << 1) ^ (0x1d & reduce_mask);
        b >>= 1;
        i += 1;
    }
    product
}

/// The `exp`-th power of the field generator, `gᵉˣᵖ`, the Q-syndrome
/// coefficient of data position `exp`. The generator has period 255, so the
/// coefficients `g⁰ … g²⁵⁴` are the 255 distinct non-zero field elements; a
/// double-parity array therefore admits at most 255 data members (the caller
/// enforces this before ever asking for a coefficient at `exp ≥ 255`).
#[must_use]
pub(crate) const fn gpow(exp: u64) -> u8 {
    let mut result: u8 = 1;
    let mut i = 0u64;
    while i < exp {
        result = mul(result, GENERATOR);
        i += 1;
    }
    result
}

/// The largest data-member count a GF(2^8) Q syndrome keeps distinct,
/// non-zero coefficients for: `g⁰ … g²⁵⁴`. A double-parity array may not have
/// more than this many data members.
pub(crate) const MAX_DATA_MEMBERS: u64 = 255;

/// The multiplicative inverse of `x` in GF(2^8). `x` must be non-zero (zero has
/// no inverse); the caller guarantees this. Computed as `x^254` (Fermat:
/// `x^(q-1) = 1`, so `x^(q-2)` is the inverse) by square-and-multiply.
#[must_use]
pub(crate) const fn inv(x: u8) -> u8 {
    // x^254 = x^(2+4+8+16+32+64+128): accumulate the odd powers.
    let mut result: u8 = 1;
    let mut base = x;
    let mut exp: u32 = 254;
    while exp > 0 {
        if exp & 1 == 1 {
            result = mul(result, base);
        }
        base = mul(base, base);
        exp >>= 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_identity_and_zero() {
        for a in 0u8..=255 {
            assert_eq!(mul(a, 0), 0);
            assert_eq!(mul(0, a), 0);
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(1, a), a);
        }
    }

    #[test]
    fn mul_is_commutative() {
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                assert_eq!(mul(a, b), mul(b, a));
            }
        }
    }

    #[test]
    fn every_nonzero_element_has_an_inverse() {
        for a in 1u8..=255 {
            let i = inv(a);
            assert_eq!(mul(a, i), 1, "inverse of {a}");
        }
    }

    #[test]
    fn mul_is_associative_and_distributive_over_xor() {
        for a in [0u8, 1, 2, 0x53, 0xca, 0xff] {
            for b in [0u8, 1, 3, 0x1d, 0x9c, 0xfe] {
                for c in [0u8, 1, 7, 0x2b, 0xa5, 0x80] {
                    assert_eq!(mul(mul(a, b), c), mul(a, mul(b, c)));
                    assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
                }
            }
        }
    }

    #[test]
    fn generator_has_full_period_255() {
        // g = 0x02 is primitive: its powers cycle through every non-zero
        // element exactly once before returning to 1.
        let mut seen = [false; 256];
        let mut x = 1u8;
        for _ in 0..255 {
            assert!(!seen[x as usize], "generator repeated before period 255");
            seen[x as usize] = true;
            x = mul(x, GENERATOR);
        }
        assert_eq!(x, 1, "generator did not return to 1 after 255 steps");
        assert!(!seen[0], "zero is never a power of the generator");
    }
}
