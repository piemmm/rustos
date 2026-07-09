//! Page-eligibility classifier for the compressed anonymous-memory
//! tier (`plans/SWAPSWAPSWAP.md` section 5).
//!
//! Only cold anonymous user pages are candidates. Everything else —
//! kernel stacks, interrupt stacks, page tables, DMA buffers, device
//! memory, driver rings, cryptographic key storage, credential or
//! capability metadata, pinned pages, latency-critical pages, and any
//! page whose role is unknown — is refused with a typed reason. The
//! classifier is a pure function, fails closed, and is the single
//! definition every compression path consults.

/// The VM's classification of what a physical page holds.
///
/// The caller (the VM layer that owns the page) supplies this honestly;
/// the classifier never guesses. [`PageKind::Unknown`] exists so a
/// caller that cannot prove a page's role has a truthful answer — and
/// that answer is always refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PageKind {
    /// An anonymous user page: heap, stack-growth, or `mem_map` memory
    /// owned by a user task. The only compressible kind.
    AnonymousUser,
    /// Clean file-backed cache: reclaimed by dropping, never worth
    /// compressing (rebuilding from the volume is cheaper).
    FileBacked,
    /// A kernel thread's stack.
    KernelStack,
    /// An interrupt or exception stack.
    InterruptStack,
    /// A page-table frame.
    PageTable,
    /// A buffer a DMA master may read or write.
    DmaBuffer,
    /// MMIO or other device memory.
    DeviceMemory,
    /// A driver ring or descriptor area shared with hardware.
    DriverRing,
    /// Storage holding cryptographic key material.
    CryptoKeyStorage,
    /// Kernel credential, token, or capability metadata.
    CredentialMetadata,
    /// The caller cannot prove the page's role.
    Unknown,
}

/// One page offered to the tier, with the attributes the eligibility
/// rules judge.
///
/// Every flag is judged independently and fail-closed: a page that is
/// anonymous but pinned, or anonymous but recently accessed, is
/// refused. The caller asserts these attributes from its own live
/// bookkeeping (page-replacement state, pin counts, DMA registry, task
/// class); the classifier trusts a *refusing* attribute uncritically
/// and an *admitting* one only in combination with every other check.
// Each bool is an independent, semantically named refusing attribute
// judged on its own; folding them into a bitset or state enum would
// obscure which live-bookkeeping fact refused the page.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PageCandidate {
    /// What the page holds.
    pub kind: PageKind,
    /// The page-replacement policy saw a recent access: the page is
    /// not cold, and compressing it would thrash.
    pub recently_accessed: bool,
    /// The page is pinned (wired) and must stay resident and mapped.
    pub pinned: bool,
    /// A DMA master may access the page independently of the CPU.
    pub dma_visible: bool,
    /// The page is marked sensitive / never-compress (it may hold
    /// secrets the zero-on-free discipline owns).
    pub sensitive: bool,
    /// The owning task is realtime, latency-critical, or otherwise
    /// marked never-compress by the scheduler's generic task class.
    pub latency_critical: bool,
}

impl PageCandidate {
    /// A cold, unpinned, CPU-only anonymous user page — the shape every
    /// admitting call site starts from before setting refusing flags.
    #[must_use]
    pub const fn cold_anonymous() -> Self {
        Self {
            kind: PageKind::AnonymousUser,
            recently_accessed: false,
            pinned: false,
            dma_visible: false,
            sensitive: false,
            latency_critical: false,
        }
    }
}

/// Why a page was refused by [`eligibility`]. Every variant is
/// fail-closed: the page stays exactly where it is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Ineligible {
    /// The page is not an anonymous user page (the refusing
    /// [`PageKind`] is carried for diagnostics).
    NotAnonymous(PageKind),
    /// The caller could not prove the page's role. Unknown is
    /// ineligible, always.
    UnknownKind,
    /// The page is not cold: the replacement policy saw a recent
    /// access.
    RecentlyAccessed,
    /// The page is pinned and must stay resident.
    Pinned,
    /// A DMA master may touch the page independently of the CPU.
    DmaVisible,
    /// The page is marked sensitive / never-compress.
    Sensitive,
    /// The owning task is realtime or latency-critical.
    LatencyCritical,
}

/// Classify one candidate. Pure and total: equal inputs always produce
/// equal outputs, and no input panics.
///
/// The checks run in a fixed order (kind first, then the refusing
/// attributes), so the reported reason is deterministic when several
/// apply.
///
/// # Errors
///
/// Returns the first applicable [`Ineligible`] reason; `Ok(())` means
/// the page may proceed to the policy gates (pressure band, caps,
/// reserves, per-task share) — eligibility alone never admits a page
/// into the tier.
pub const fn eligibility(candidate: &PageCandidate) -> Result<(), Ineligible> {
    match candidate.kind {
        PageKind::AnonymousUser => {}
        PageKind::Unknown => return Err(Ineligible::UnknownKind),
        kind => return Err(Ineligible::NotAnonymous(kind)),
    }
    if candidate.recently_accessed {
        return Err(Ineligible::RecentlyAccessed);
    }
    if candidate.pinned {
        return Err(Ineligible::Pinned);
    }
    if candidate.dma_visible {
        return Err(Ineligible::DmaVisible);
    }
    if candidate.sensitive {
        return Err(Ineligible::Sensitive);
    }
    if candidate.latency_critical {
        return Err(Ineligible::LatencyCritical);
    }
    Ok(())
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn cold_anonymous_page_is_eligible() {
        assert_eq!(eligibility(&PageCandidate::cold_anonymous()), Ok(()));
    }

    #[test]
    fn every_non_anonymous_kind_is_refused() {
        let refused = [
            PageKind::FileBacked,
            PageKind::KernelStack,
            PageKind::InterruptStack,
            PageKind::PageTable,
            PageKind::DmaBuffer,
            PageKind::DeviceMemory,
            PageKind::DriverRing,
            PageKind::CryptoKeyStorage,
            PageKind::CredentialMetadata,
        ];
        for kind in refused {
            let candidate = PageCandidate {
                kind,
                ..PageCandidate::cold_anonymous()
            };
            assert_eq!(
                eligibility(&candidate),
                Err(Ineligible::NotAnonymous(kind)),
                "{kind:?} must be refused"
            );
        }
    }

    #[test]
    fn unknown_kind_fails_closed() {
        let candidate = PageCandidate {
            kind: PageKind::Unknown,
            ..PageCandidate::cold_anonymous()
        };
        assert_eq!(eligibility(&candidate), Err(Ineligible::UnknownKind));
    }

    #[test]
    fn each_refusing_attribute_is_decisive_alone() {
        let cases = [
            (
                PageCandidate {
                    recently_accessed: true,
                    ..PageCandidate::cold_anonymous()
                },
                Ineligible::RecentlyAccessed,
            ),
            (
                PageCandidate {
                    pinned: true,
                    ..PageCandidate::cold_anonymous()
                },
                Ineligible::Pinned,
            ),
            (
                PageCandidate {
                    dma_visible: true,
                    ..PageCandidate::cold_anonymous()
                },
                Ineligible::DmaVisible,
            ),
            (
                PageCandidate {
                    sensitive: true,
                    ..PageCandidate::cold_anonymous()
                },
                Ineligible::Sensitive,
            ),
            (
                PageCandidate {
                    latency_critical: true,
                    ..PageCandidate::cold_anonymous()
                },
                Ineligible::LatencyCritical,
            ),
        ];
        for (candidate, reason) in cases {
            assert_eq!(eligibility(&candidate), Err(reason));
        }
    }

    #[test]
    fn kind_refusal_wins_over_attribute_refusals() {
        // Deterministic reason order: the kind check runs first.
        let candidate = PageCandidate {
            kind: PageKind::DmaBuffer,
            pinned: true,
            sensitive: true,
            ..PageCandidate::cold_anonymous()
        };
        assert_eq!(
            eligibility(&candidate),
            Err(Ineligible::NotAnonymous(PageKind::DmaBuffer))
        );
    }

    #[test]
    fn classification_is_deterministic_for_equal_inputs() {
        let candidate = PageCandidate {
            sensitive: true,
            ..PageCandidate::cold_anonymous()
        };
        assert_eq!(eligibility(&candidate), eligibility(&candidate));
    }
}
