//! Platform entropy-source surface of the Arch HAL.
//!
//! TAIRiX has exactly one kernel cryptographic random subsystem, and it is
//! useless until it is *seeded*: a CSPRNG with no entropy input produces no
//! output (`random_get` fails closed, process-instance ids lose their
//! unpredictable half, no key or nonce can be drawn). Raw entropy enters the
//! system through `lib/rng`'s [`EntropySource`](tairix_rng::EntropySource)
//! seam, and the highest-quality, lowest-latency input on most machines is a
//! CPU instruction the architecture exposes: x86 `RDSEED`/`RDRAND`, ARMv8.5
//! `RNDR`/`RNDRRS`, the RISC-V `Zkr` `seed` CSR. Only the architecture port
//! can issue that instruction, so — exactly like the [`super::memtag`] and
//! [`super::sidechannel`] surfaces — the platform entropy source is a closed
//! trait on the Arch HAL.
//!
//! # What lives here
//!
//! * [`PlatformEntropy`] — the per-port handle the kernel reaches through to
//!   draw raw hardware entropy. It is a [`HardwareRng`] (so `lib/rng` can mix
//!   it through [`HardwareEntropy`](tairix_rng::HardwareEntropy) and
//!   [`CombinedSource`](tairix_rng::CombinedSource) before it ever feeds the
//!   DRBG — hardware output is *input material*, never final output) plus an
//!   honest [`EntropyProfile`] declaration.
//! * [`EntropyProfile`] / [`EntropySupport`] — the honest declaration,
//!   exactly like [`super::memtag::TaggingProfile`] /
//!   [`super::memtag::Tagging`]: a port's hardware-RNG path is
//!   [`EntropySupport::Supported`], [`EntropySupport::Unsupported`] (the
//!   silicon genuinely exposes no entropy instruction, justified), or
//!   [`EntropySupport::Pending`] (the silicon supports it but a not-yet-landed
//!   privilege/wiring step must enable it).
//! * [`conformance`] — the conformance vertical every port runs against its
//!   handle.
//!
//! # Why a port may have no usable source
//!
//! A [`EntropySupport::Supported`] declaration means the port *implements* an
//! entropy instruction; it does **not** promise that instruction is present
//! and enabled on every machine that arch runs on. Runtime feature detection
//! (CPUID, an ID register, the QEMU CPU model) decides whether
//! [`HardwareRng::try_fill`] actually produces bytes; when it cannot, the draw
//! fails closed with [`EntropyError::Unavailable`] and the kernel reserve
//! simply stays unseeded rather than weakening to predictable output. The
//! charter forbids trusting any single source alone, so a port's hardware RNG
//! is one mixed input — additional software sources (boot-time timing jitter,
//! an interrupt-arrival pool) are a tracked follow-up, not built here.

use tairix_rng::{EntropyError, HardwareRng};

/// One entropy feature's status on a given port.
///
/// Mirrors [`super::memtag::Tagging`]: a port takes exactly one honest
/// position. [`EntropySupport::Unsupported`] is permitted **only** where the
/// silicon genuinely exposes no entropy source, and the payload must record
/// why. [`EntropySupport::Pending`] is for silicon that *does* expose a source
/// but where a not-yet-landed step (a privilege-mode delegation, a host
/// import) must enable it; a `Pending` feature is honest and tracked but is
/// not release-ready.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EntropySupport {
    /// The port implements a hardware entropy instruction. Whether it
    /// produces bytes on a given machine is decided at runtime by feature
    /// detection; a draw that cannot be satisfied fails closed.
    Supported,
    /// The port's silicon exposes no entropy source. The payload is the
    /// justification recorded in the port's `README.md`; it must be non-empty.
    Unsupported(&'static str),
    /// The silicon exposes a source, but it cannot be used yet because it
    /// depends on a step that has not landed. The payload is the tracking
    /// note; it must be non-empty.
    Pending(&'static str),
}

impl EntropySupport {
    /// `true` if this feature is [`EntropySupport::Supported`].
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// `true` if this feature is a tracked [`EntropySupport::Pending`].
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// `true` if this feature is release-ready: supported or a justified
    /// [`EntropySupport::Unsupported`]. A [`EntropySupport::Pending`] gap is
    /// not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Supported | Self::Unsupported(_))
    }

    /// The explanatory note for a non-supported decision, or `None` when
    /// supported.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// A port's honest declaration of the entropy source it drives.
///
/// One genuinely distinct property, so one slot (no slot exists that the
/// kernel does not need): whether the port exposes a hardware entropy
/// instruction.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EntropyProfile {
    /// A hardware entropy instruction is implemented (e.g. x86 `RDSEED`,
    /// ARMv8.5 `RNDR`).
    pub hardware_rng: EntropySupport,
}

/// A single named slot of an [`EntropyProfile`], yielded by
/// [`EntropyProfile::entries`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EntropyEntry {
    /// Stable, human-readable name of the slot.
    pub name: &'static str,
    /// The port's decision for this slot.
    pub support: EntropySupport,
}

/// Reason an [`EntropyProfile`] failed [`EntropyProfile::validate`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    /// A non-supported decision carried an empty (or whitespace-only)
    /// justification. The charter requires every omission to be justified;
    /// `field` names the offending slot.
    EmptyJustification {
        /// The [`EntropyEntry::name`] of the unjustified slot.
        field: &'static str,
    },
}

impl EntropyProfile {
    /// The entropy slots, in a stable order, each paired with its name.
    #[must_use]
    pub const fn entries(&self) -> [EntropyEntry; 1] {
        [EntropyEntry {
            name: "hardware_rng",
            support: self.hardware_rng,
        }]
    }

    /// Validate the honesty rule: every non-supported feature must carry a
    /// non-empty explanation.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::EmptyJustification`] naming the first slot
    /// whose [`EntropySupport::detail`] is present but empty or
    /// whitespace-only.
    pub fn validate(&self) -> Result<(), ProfileError> {
        for entry in self.entries() {
            if let Some(reason) = entry.support.detail() {
                if reason.trim().is_empty() {
                    return Err(ProfileError::EmptyJustification { field: entry.name });
                }
            }
        }
        Ok(())
    }

    /// `true` if every feature is release-ready (supported or a justified
    /// [`EntropySupport::Unsupported`], with no [`EntropySupport::Pending`]
    /// gap remaining).
    #[must_use]
    pub fn is_release_ready(&self) -> bool {
        self.entries()
            .iter()
            .all(|entry| entry.support.is_release_ready())
    }

    /// `true` if the port implements a hardware entropy instruction the
    /// kernel should attempt to seed its reserve from.
    #[must_use]
    pub fn provides_hardware_entropy(&self) -> bool {
        self.hardware_rng.is_supported()
    }
}

/// The platform-entropy handle an architecture port exposes.
///
/// The kernel draws raw entropy through the inherited
/// [`HardwareRng::try_fill`]; that output is conditioned by `lib/rng`'s DRBG
/// before any caller sees it (hardware output is never final output). The
/// honest [`Self::profile`] declares whether the port implements a source at
/// all and must satisfy [`EntropyProfile::validate`].
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the handle
/// from boot and (on reseed) from any CPU.
pub trait PlatformEntropy: HardwareRng + Send + Sync {
    /// The port's honest declaration of the entropy source it drives.
    /// Must satisfy [`EntropyProfile::validate`].
    fn profile(&self) -> EntropyProfile;
}

/// The platform-entropy conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`PlatformEntropy`] handle. The suite is portable — it names only the
/// trait — and runs on the host, exactly like the [`super::memtag`] vertical:
/// it is the trait-level "profile is honest" / "a draw is callable and fails
/// closed" check. Each port's own host tests additionally pin the concrete
/// profile its silicon requires.
pub mod conformance {
    use super::{EntropyError, PlatformEntropy};

    /// Run the entire platform-entropy conformance suite against `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if any required property does not hold: the
    /// profile fails [`super::EntropyProfile::validate`], or a draw against an
    /// [`super::EntropySupport::Unsupported`] port does not fail closed.
    pub fn run_all<E: PlatformEntropy + ?Sized>(port: &E) {
        profile_is_honest(port);
        draw_is_callable_and_fails_closed(port);
    }

    /// The profile validates and every non-supported feature carries a
    /// non-empty justification.
    fn profile_is_honest<E: PlatformEntropy + ?Sized>(port: &E) {
        let profile = port.profile();
        assert!(
            profile.validate().is_ok(),
            "entropy profile must justify every non-supported feature: {:?}",
            profile.validate()
        );
        for entry in profile.entries() {
            if let Some(reason) = entry.support.detail() {
                assert!(
                    !reason.trim().is_empty(),
                    "non-supported feature `{}` must carry a non-empty explanation",
                    entry.name
                );
            }
        }
    }

    /// A draw is callable without panicking. A port that declares it exposes
    /// **no** source must fail the draw closed (never silently "succeed" with
    /// predictable bytes); a port that implements a source may succeed or
    /// fail closed depending on runtime availability (the instruction is
    /// `cfg`-gated off-target, or absent on this machine), and both are
    /// acceptable here — the kernel reserve seeds only when a draw actually
    /// produces bytes.
    fn draw_is_callable_and_fails_closed<E: PlatformEntropy + ?Sized>(port: &E) {
        let mut out = [0u8; 32];
        let result = port.try_fill(&mut out);
        if !port.profile().hardware_rng.is_supported() {
            assert!(
                matches!(result, Err(EntropyError::Unavailable)),
                "a port with no hardware entropy source must fail a draw closed, got {result:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubPort {
        profile: EntropyProfile,
        fills: bool,
    }

    impl HardwareRng for StubPort {
        fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError> {
            if self.fills {
                // Deterministic non-zero pattern via a wrapping `u8` counter
                // (no `usize`-to-`u8` cast).
                let mut acc: u8 = 0x5A;
                for b in out.iter_mut() {
                    *b = acc;
                    acc = acc.wrapping_add(0x11);
                }
                Ok(())
            } else {
                Err(EntropyError::Unavailable)
            }
        }
    }

    impl PlatformEntropy for StubPort {
        fn profile(&self) -> EntropyProfile {
            self.profile
        }
    }

    fn supported(fills: bool) -> StubPort {
        StubPort {
            profile: EntropyProfile {
                hardware_rng: EntropySupport::Supported,
            },
            fills,
        }
    }

    fn unsupported() -> StubPort {
        StubPort {
            profile: EntropyProfile {
                hardware_rng: EntropySupport::Unsupported("no entropy instruction on this silicon"),
            },
            fills: false,
        }
    }

    #[test]
    fn support_helpers() {
        assert!(EntropySupport::Supported.is_supported());
        assert!(EntropySupport::Pending("x").is_pending());
        assert!(!EntropySupport::Supported.is_pending());
        assert_eq!(EntropySupport::Supported.detail(), None);
        assert_eq!(EntropySupport::Unsupported("why").detail(), Some("why"));
        assert!(EntropySupport::Unsupported("why").is_release_ready());
        assert!(!EntropySupport::Pending("later").is_release_ready());
    }

    #[test]
    fn supported_profile_validates_and_is_release_ready() {
        let p = supported(true).profile();
        assert_eq!(p.validate(), Ok(()));
        assert!(p.is_release_ready());
        assert!(p.provides_hardware_entropy());
    }

    #[test]
    fn justified_unsupported_validates_but_provides_nothing() {
        let p = unsupported().profile();
        assert_eq!(p.validate(), Ok(()));
        assert!(p.is_release_ready());
        assert!(!p.provides_hardware_entropy());
    }

    #[test]
    fn empty_justification_is_rejected() {
        let p = EntropyProfile {
            hardware_rng: EntropySupport::Unsupported("   "),
        };
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "hardware_rng"
            })
        );
    }

    #[test]
    fn empty_pending_note_is_rejected() {
        let p = EntropyProfile {
            hardware_rng: EntropySupport::Pending(""),
        };
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "hardware_rng"
            })
        );
    }

    #[test]
    fn pending_is_honest_but_not_release_ready() {
        let p = EntropyProfile {
            hardware_rng: EntropySupport::Pending("Zkr seed CSR needs M-mode delegation"),
        };
        assert_eq!(p.validate(), Ok(()));
        assert!(!p.is_release_ready());
        assert!(!p.provides_hardware_entropy());
    }

    #[test]
    fn entries_round_trip_the_named_slot() {
        let p = supported(true).profile();
        assert_eq!(p.entries()[0].name, "hardware_rng");
    }

    #[test]
    fn conformance_accepts_a_supported_port_that_fills() {
        let port = supported(true);
        conformance::run_all(&port);
        let dynamic: &dyn PlatformEntropy = &port;
        conformance::run_all(dynamic);
    }

    #[test]
    fn conformance_accepts_a_supported_port_that_cannot_fill_at_runtime() {
        // A Supported port whose instruction is unavailable right now fails
        // closed; conformance still accepts it (the reserve simply will not
        // seed from it).
        conformance::run_all(&supported(false));
    }

    #[test]
    fn conformance_accepts_an_unsupported_port_that_fails_closed() {
        conformance::run_all(&unsupported());
    }

    #[test]
    #[should_panic(expected = "must justify every non-supported feature")]
    fn conformance_rejects_an_unjustified_claim() {
        let port = StubPort {
            profile: EntropyProfile {
                hardware_rng: EntropySupport::Unsupported(""),
            },
            fills: false,
        };
        conformance::run_all(&port);
    }

    #[test]
    #[should_panic(expected = "must fail a draw closed")]
    fn conformance_rejects_an_unsupported_port_that_pretends_to_fill() {
        // An Unsupported port that returns bytes is a lie the suite must catch.
        let port = StubPort {
            profile: EntropyProfile {
                hardware_rng: EntropySupport::Unsupported("claims none"),
            },
            fills: true,
        };
        conformance::run_all(&port);
    }
}
