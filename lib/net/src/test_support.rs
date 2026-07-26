//! Test-only deterministic [`TempAddrSource`] for the engine tests.
//!
//! The RFC 8981 temporary-address path draws its randomised interface
//! identifiers and desync jitter from an injected [`TempAddrSource`]
//! (entropy stays at the service seam, the engine is pure). The tests
//! inject this reproducible splitmix64 stream so the engine's behaviour
//! is fully deterministic while still yielding distinct, non-reserved
//! identifiers.

use alloc::boxed::Box;

use crate::iface::TempAddrSource;

/// A deterministic randomness source: a splitmix64 stream, so
/// temporary identifiers are distinct across draws yet reproducible
/// run to run.
#[derive(Debug)]
pub(crate) struct SeqTempSource {
    state: u64,
}

impl SeqTempSource {
    /// A source seeded with the splitmix64 golden ratio constant.
    pub(crate) fn new() -> Self {
        Self::seeded(0x9E37_79B9_7F4A_7C15)
    }

    /// A source with a caller-chosen seed, so a test can force a
    /// distinct identifier stream.
    pub(crate) fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl TempAddrSource for SeqTempSource {
    fn fill_random(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            let len = chunk.len();
            chunk.copy_from_slice(&bytes[..len]);
        }
    }
}

/// A boxed deterministic source for `Iface::new` / `Stack::new` in
/// tests that do not exercise privacy addresses.
pub(crate) fn temp_source() -> Box<dyn TempAddrSource> {
    Box::new(SeqTempSource::new())
}
