//! WASM-linear-memory isolation model — the wasm32 "MMU" analogue.
//!
//! The bare-metal ports enforce process isolation with hardware page
//! tables: an [`crate::kernel_arch::WasmArch`] process can only reach
//! another's memory through an explicit, capability-checked shared
//! mapping (`AGENTS.md` §4). wasm32 gets the same guarantee from the
//! WebAssembly sandbox: each Web Worker runs a distinct module instance
//! with its **own linear memory**, and a load/store is bounds-checked by
//! the engine against that instance's memory — a worker cannot even name
//! another worker's bytes.
//!
//! This module is the architecture-neutral *model* of that boundary. A
//! [`MemoryRegion`] names one worker's linear-memory span; an
//! [`AddressSpace`] wraps the region a context is allowed to touch and
//! rejects any access that strays outside it with a [`WasmFault`] — the
//! wasm32 equivalent of the page-fault the bare-metal ports raise. The
//! browser "memory-isolation test passes" vertical builds a victim and
//! an attacker `AddressSpace` over disjoint regions and confirms the
//! attacker faults on a victim-only address.
//!
//! # Host testability
//!
//! The whole model is plain integer arithmetic with no wasm intrinsics,
//! so it builds and is unit-tested on the host. All arithmetic is
//! checked: an access whose `addr + len` would overflow is a fault, not
//! a wraparound (`AGENTS.md` §2.9).

/// A contiguous span of one worker's WASM linear memory.
///
/// `base` is the span's start address in the model's flat coordinate
/// space and `len` its byte length. Distinct workers own disjoint
/// regions, mirroring the distinct linear memories of distinct module
/// instances.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    /// First address of the region.
    pub base: u64,
    /// Length of the region in bytes.
    pub len: u64,
}

impl MemoryRegion {
    /// Construct a region of `len` bytes starting at `base`.
    #[must_use]
    pub const fn new(base: u64, len: u64) -> Self {
        Self { base, len }
    }

    /// One past the last address of the region, saturating at
    /// [`u64::MAX`] so a region declared at the top of the address space
    /// cannot wrap (`AGENTS.md` §2.9).
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base.saturating_add(self.len)
    }

    /// `true` iff the `[addr, addr + len)` access lies wholly within this
    /// region. A zero-length access is contained iff its address is in
    /// range. An access whose end would overflow is never contained.
    #[must_use]
    pub const fn contains(&self, addr: u64, len: u64) -> bool {
        let Some(access_end) = addr.checked_add(len) else {
            return false;
        };
        addr >= self.base && access_end <= self.end()
    }
}

/// An access that fell outside the [`AddressSpace`]'s region — the
/// wasm32 analogue of a page fault.
///
/// Carries the attempted access and the region that rejected it so the
/// fault handler (and the isolation test) can confirm exactly what was
/// denied.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WasmFault {
    /// Start address of the rejected access.
    pub addr: u64,
    /// Byte length of the rejected access.
    pub len: u64,
    /// The region the faulting context was confined to.
    pub region: MemoryRegion,
}

/// The set of addresses a single worker context is permitted to touch.
///
/// Exactly one [`MemoryRegion`] — the context's own linear memory. Any
/// access outside it is a [`WasmFault`]; there is no ambient way to
/// reach another context's region (`AGENTS.md` §4 — no ambient
/// authority). Cross-context sharing would be a separate, explicit,
/// capability-checked object, never an implicit reach.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AddressSpace {
    region: MemoryRegion,
}

impl AddressSpace {
    /// Confine a context to `region`.
    #[must_use]
    pub const fn new(region: MemoryRegion) -> Self {
        Self { region }
    }

    /// The region this context is confined to.
    #[must_use]
    pub const fn region(&self) -> MemoryRegion {
        self.region
    }

    /// Check a `[addr, addr + len)` access against the confined region.
    ///
    /// Returns the in-region byte offset (`addr - base`) on success, so
    /// the caller can index its own linear memory; returns a
    /// [`WasmFault`] for any access that strays outside the region or
    /// whose end would overflow. This is the single check every access
    /// goes through — there is no unchecked fast path (`AGENTS.md`
    /// §5.4.3 — validate every input, fail closed).
    ///
    /// # Errors
    ///
    /// [`WasmFault`] if the access is not wholly contained in the region.
    pub const fn check_access(&self, addr: u64, len: u64) -> Result<u64, WasmFault> {
        if self.region.contains(addr, len) {
            Ok(addr - self.region.base)
        } else {
            Err(WasmFault {
                addr,
                len,
                region: self.region,
            })
        }
    }

    /// `true` iff a single-byte access at `addr` is permitted.
    #[must_use]
    pub const fn can_read(&self, addr: u64) -> bool {
        self.region.contains(addr, 1)
    }
}

/// The [`MemoryRegion`] covering this worker instance's *actual* WASM
/// linear memory, `[0, memory_size_in_bytes)`.
///
/// Reads the live linear-memory size from the engine
/// (`memory.size` × the 64 KiB WASM page) so the isolation check is tied
/// to the real bytes this instance owns, not a synthetic span: an
/// `AddressSpace` built from it accepts every byte the engine would let
/// this instance load and rejects every address beyond it — including any
/// address that belongs to another worker's separate linear memory. Each
/// Web Worker runs this against its own memory, so the check is genuinely
/// *per worker* (`AGENTS.md` §4 — no ambient cross-context reach).
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn live_memory_region() -> MemoryRegion {
    /// WebAssembly fixes the linear-memory page at 64 KiB.
    const WASM_PAGE_BYTES: u64 = 64 * 1024;
    let pages = core::arch::wasm32::memory_size(0) as u64;
    MemoryRegion::new(0, pages.saturating_mul(WASM_PAGE_BYTES))
}

#[cfg(test)]
#[path = "isolation_tests.rs"]
mod tests;
