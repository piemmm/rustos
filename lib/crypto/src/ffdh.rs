//! Finite-field Diffie-Hellman over the RFC 3526 MODP groups.
//!
//! SSH's `diffie-hellman-group{14,16,18}-*` and its group exchange
//! (RFC 4419) need `g^x mod p` over a safe-prime group. No vetted pure-Rust
//! FFDH crate exists, so the group operation is composed here from the
//! constant-time Montgomery exponentiation of the audited `crypto-bigint`
//! that the NIST curve stack in [`crate::nistp`] already depends on — the
//! same shape as PBKDF2 over the audited HMAC in [`crate::kdf`]: a standard
//! construction over an audited primitive rather than a hand-rolled one.
//!
//! # Peer public-value validation
//!
//! A peer value is accepted only when `1 < y < p - 1`. For a safe-prime
//! group whose private exponent is ephemeral and used once — which is every
//! SSH key exchange — that is the partial validation NIST SP 800-56A Rev3
//! sanctions, and it excludes every small subgroup the group has (`{1}` and
//! `{1, p-1}`; a safe prime has no others). Full validation would cost a
//! second exponentiation per handshake to exclude a peer choosing a
//! generator of the order-`2q` group, which leaks at most the parity of one
//! ephemeral exponent through a value that is hashed before use.
//!
//! # The exponent is the caller's
//!
//! As everywhere in this crate, nothing here draws randomness. The caller
//! supplies the private exponent and owns wiping its copy.
//!
//! # Cost, and why the group set is closed
//!
//! One exponentiation over the 8192-bit group holds a sixteen-entry
//! windowing table of full-width integers — about 16 KiB — plus the
//! Montgomery parameters. Accepting an arbitrary caller-supplied modulus
//! width would make that cost unbounded and would need a primality test to
//! validate the group, so only the three RFC 3526 groups are offered.

use core::fmt;

use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, Uint, U2048, U4096, U8192};

/// A finite-field Diffie-Hellman operation was refused.
///
/// Opaque and single-variant, as elsewhere in this crate: every cause is an
/// exponent or peer value the caller must treat as a refused handshake.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct FfdhError(());

impl fmt::Display for FfdhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("finite-field diffie-hellman refused")
    }
}

/// The generator every RFC 3526 MODP group fixes.
const GENERATOR: u8 = 2;

/// Emit the modulus constant, the value type, and the two operations for one
/// RFC 3526 MODP group.
///
/// The groups differ only in their modulus and its width, so the operation
/// is written once rather than three times.
macro_rules! modp_group {
    (
        $uint:ty,
        $name:literal,
        $where:literal,
        $len:ident = $bytes:literal,
        $value_ty:ident,
        $modulus:ident,
        $public:ident,
        $agree:ident,
        $prime:expr
    ) => {
        #[doc = concat!("Length, in bytes, of a ", $name, " value: the modulus width.")]
        pub const $len: usize = $bytes;

        #[doc = concat!("A ", $name, " public value or shared secret, big-endian")]
        /// and zero-padded to the modulus width.
        pub type $value_ty = [u8; $len];

        #[doc = concat!("The ", $name, " prime modulus (", $where, ").")]
        ///
        /// A safe prime: `(p - 1) / 2` is also prime, so the only subgroups
        /// are the trivial ones the peer range check already excludes.
        pub const $modulus: $uint = <$uint>::from_be_hex($prime);

        // Montgomery arithmetic needs an odd modulus, and every prime is
        // odd; a mistyped constant is caught here rather than at the call.
        const _: () = assert!($modulus.as_words()[0] & 1 == 1);

        #[doc = concat!("Compute this side's ", $name, " public value, `g^x mod p`.")]
        ///
        /// # Errors
        ///
        /// Returns [`FfdhError`] if the exponent is zero or is not below the
        /// modulus; neither is a private key.
        pub fn $public(exponent: &$value_ty) -> Result<$value_ty, FfdhError> {
            let x = <$uint>::from_be_slice(exponent);
            check_exponent(&x, &$modulus)?;
            let params = group_params(&$modulus);
            let g = FixedMontyForm::new(&<$uint>::from_u8(GENERATOR), &params);
            Ok(to_value(&g.pow(&x).retrieve()))
        }

        #[doc = concat!("Agree a ", $name, " shared secret, `peer^x mod p`.")]
        ///
        /// # Errors
        ///
        /// Returns [`FfdhError`] if the exponent is not a private key, or if
        /// the peer value is outside `1 < y < p - 1`, whose excluded ends
        /// are the group's small subgroups.
        pub fn $agree(exponent: &$value_ty, peer: &$value_ty) -> Result<$value_ty, FfdhError> {
            let x = <$uint>::from_be_slice(exponent);
            let y = <$uint>::from_be_slice(peer);
            check_exponent(&x, &$modulus)?;
            check_peer(&y, &$modulus)?;
            let params = group_params(&$modulus);
            Ok(to_value(
                &FixedMontyForm::new(&y, &params).pow(&x).retrieve(),
            ))
        }
    };
}

/// Montgomery parameters for a group modulus.
///
/// Every RFC 3526 prime is odd, and every caller passes one of the three
/// constants below, so the `Odd` wrapper cannot reject one. The
/// compile-time assertion beside each modulus is what keeps that true: a
/// mistyped constant fails the build rather than this call.
fn group_params<const LIMBS: usize>(modulus: &Uint<LIMBS>) -> FixedMontyParams<LIMBS> {
    // SAFETY-INVARIANT: an even modulus is rejected at compile time where
    // each group constant is declared, so this cannot fail.
    let odd = Odd::new(*modulus).expect("an RFC 3526 modulus is odd by construction");
    FixedMontyParams::new(odd)
}

/// Render a group element as a fixed-width big-endian byte string.
///
/// The width is the modulus width, so a shared secret with leading zero
/// bytes keeps them — SSH re-encodes it as an `mpint`, and a caller that
/// received a short string could not tell a stripped value from a smaller
/// one.
fn to_value<const LIMBS: usize, const BYTES: usize>(value: &Uint<LIMBS>) -> [u8; BYTES] {
    let mut out = [0u8; BYTES];
    out.copy_from_slice(value.to_be_bytes().as_ref());
    out
}

/// A private exponent must satisfy `0 < x < p`.
fn check_exponent<const LIMBS: usize>(
    x: &Uint<LIMBS>,
    modulus: &Uint<LIMBS>,
) -> Result<(), FfdhError> {
    if bool::from(x.is_zero()) || x >= modulus {
        return Err(FfdhError(()));
    }
    Ok(())
}

/// A peer public value must satisfy `1 < y < p - 1`.
///
/// The excluded ends are exactly the group's small subgroups: `y = 1`
/// generates `{1}` and `y = p - 1` generates `{1, p - 1}`, either of which
/// would let the peer dictate the shared secret instead of contributing to
/// it. `y = 0` falls under the same lower bound.
fn check_peer<const LIMBS: usize>(y: &Uint<LIMBS>, modulus: &Uint<LIMBS>) -> Result<(), FfdhError> {
    let one = Uint::<LIMBS>::ONE;
    if y <= &one || y >= &modulus.wrapping_sub(&one) {
        return Err(FfdhError(()));
    }
    Ok(())
}

modp_group!(
    U2048,
    "2048-bit MODP group",
    "RFC 3526 section 3, SSH group 14",
    FFDH_GROUP14_LEN = 256,
    Ffdh2048Value,
    FFDH_GROUP14_MODULUS,
    ffdh_group14_public,
    ffdh_group14_agree,
    concat!(
        "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
        "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
        "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
        "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
        "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
        "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
        "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
        "3995497CEA956AE515D2261898FA051015728E5A8AACAA68FFFFFFFFFFFFFFFF"
    )
);

modp_group!(
    U4096,
    "4096-bit MODP group",
    "RFC 3526 section 5, SSH group 16",
    FFDH_GROUP16_LEN = 512,
    Ffdh4096Value,
    FFDH_GROUP16_MODULUS,
    ffdh_group16_public,
    ffdh_group16_agree,
    concat!(
        "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
        "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
        "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
        "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
        "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
        "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
        "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
        "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
        "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
        "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
        "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
        "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A92108011A723C12A787E6D7",
        "88719A10BDBA5B2699C327186AF4E23C1A946834B6150BDA2583E9CA2AD44CE8",
        "DBBBC2DB04DE8EF92E8EFC141FBECAA6287C59474E6BC05D99B2964FA090C3A2",
        "233BA186515BE7ED1F612970CEE2D7AFB81BDD762170481CD0069127D5B05AA9",
        "93B4EA988D8FDDC186FFB7DC90A6C08F4DF435C934063199FFFFFFFFFFFFFFFF"
    )
);

modp_group!(
    U8192,
    "8192-bit MODP group",
    "RFC 3526 section 7, SSH group 18",
    FFDH_GROUP18_LEN = 1024,
    Ffdh8192Value,
    FFDH_GROUP18_MODULUS,
    ffdh_group18_public,
    ffdh_group18_agree,
    concat!(
        "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
        "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
        "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
        "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
        "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
        "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
        "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
        "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
        "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
        "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
        "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
        "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A92108011A723C12A787E6D7",
        "88719A10BDBA5B2699C327186AF4E23C1A946834B6150BDA2583E9CA2AD44CE8",
        "DBBBC2DB04DE8EF92E8EFC141FBECAA6287C59474E6BC05D99B2964FA090C3A2",
        "233BA186515BE7ED1F612970CEE2D7AFB81BDD762170481CD0069127D5B05AA9",
        "93B4EA988D8FDDC186FFB7DC90A6C08F4DF435C93402849236C3FAB4D27C7026",
        "C1D4DCB2602646DEC9751E763DBA37BDF8FF9406AD9E530EE5DB382F413001AE",
        "B06A53ED9027D831179727B0865A8918DA3EDBEBCF9B14ED44CE6CBACED4BB1B",
        "DB7F1447E6CC254B332051512BD7AF426FB8F401378CD2BF5983CA01C64B92EC",
        "F032EA15D1721D03F482D7CE6E74FEF6D55E702F46980C82B5A84031900B1C9E",
        "59E7C97FBEC7E8F323A97A7E36CC88BE0F1D45B7FF585AC54BD407B22B4154AA",
        "CC8F6D7EBF48E1D814CC5ED20F8037E0A79715EEF29BE32806A1D58BB7C5DA76",
        "F550AA3D8A1FBFF0EB19CCB1A313D55CDA56C9EC2EF29632387FE8D76E3C0468",
        "043E8F663F4860EE12BF2D5B0B7474D6E694F91E6DBE115974A3926F12FEE5E4",
        "38777CB6A932DF8CD8BEC4D073B931BA3BC832B68D9DD300741FA7BF8AFC47ED",
        "2576F6936BA424663AAB639C5AE4F5683423B4742BF1C978238F16CBE39D652D",
        "E3FDB8BEFC848AD922222E04A4037C0713EB57A81A23F0C73473FC646CEA306B",
        "4BCBC8862F8385DDFA9D4B7FA2C087E879683303ED5BDD3A062B3CF5B3A278A6",
        "6D2A13F83F44F82DDF310EE074AB6A364597E899A0255DC164F31CC50846851D",
        "F9AB48195DED7EA1B1D510BD7EE74D73FAF36BC31ECFA268359046F4EB879F92",
        "4009438B481C6CD7889A002ED5EE382BC9190DA6FC026E479558E4475677E9AA",
        "9E3050E2765694DFC81F56E880B96E7160C980DD98EDD3DFFFFFFFFFFFFFFFFF"
    )
);

#[cfg(test)]
#[path = "ffdh_tests.rs"]
mod tests;
