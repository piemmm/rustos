//! The two `abi-v1` syscalls [`RtDriverHost`](crate::RtDriverHost) issues,
//! behind a host-testable seam.
//!
//! [`GrantSyscalls`] abstracts the `mmio_map` and `dma_alloc` syscall
//! wrappers so the host's grant-resolution and address-translation logic is
//! unit-tested without a kernel — exactly as the bus drivers
//! mock their register windows. Production driver processes use
//! [`RtGrantSyscalls`], the zero-sized forwarder to `tairix_rt`, so the one
//! syscall trap is not duplicated.

/// The `mmio_map` / `dma_alloc` syscalls the user-space driver host issues.
///
/// Both methods return the kernel's raw signed `abi-v1` result register: a
/// non-negative value is the base **user virtual address** of the mapped
/// window or carved buffer, and a negative value is `-errno` (recover the
/// [`tairix_abi::Errno`] discriminant as `-ret`). The host
/// adds no authority; the kernel validates the grant handle and every bound on
/// the far side of the trap.
pub trait GrantSyscalls {
    /// Map the `[offset, offset + len)` sub-region of the device MMIO window
    /// named by the kernel-issued grant `handle` into the calling task's own
    /// address space, returning the base user virtual address of the mapped
    /// sub-region (or `-errno`).
    ///
    /// Mapping a sub-region (not the whole grant) is what lets a driver
    /// granted a large outbound bus aperture map just the single BAR it
    /// enumerated rather than the entire window.
    ///
    /// Mirrors [`tairix_rt::mmio_map`].
    fn mmio_map(&self, handle: u64, offset: u64, len: usize) -> i64;

    /// The kernel-attested machine summary, or `None` when the call
    /// refused. Drivers size their pinned DMA staging from it.
    ///
    /// Mirrors [`tairix_rt::boot_facts`].
    fn boot_facts(&self) -> Option<tairix_abi::BootFacts>;

    /// Carve a coherent DMA buffer of `len` bytes bounded by the constraint
    /// named by the kernel-issued grant `handle`, write the buffer's
    /// device-visible base to `device_out`, and return its base user virtual
    /// address (or `-errno`; `device_out` is left untouched on a negative
    /// result).
    ///
    /// Mirrors [`tairix_rt::dma_alloc`].
    fn dma_alloc(&self, handle: u64, len: usize, device_out: &mut u64) -> i64;

    /// Release the coherent DMA buffer based at `cpu_va` (the user virtual
    /// base a prior [`Self::dma_alloc`] returned) bounded by the constraint
    /// named by grant `handle`, returning `0` on success (or `-errno`). The
    /// symmetric free for [`Self::dma_alloc`]: a long-running driver reclaims
    /// each transfer's buffers through this rather than leaking DMA frames
    /// until it exits.
    ///
    /// Mirrors [`tairix_rt::dma_free`].
    fn dma_free(&self, handle: u64, cpu_va: u64) -> i64;

    /// Map the cross-process shared-memory region named by the kernel-issued
    /// grant `handle` into the calling task's own address space, returning the
    /// base user virtual address of the mapping (or `-errno`) and writing the
    /// region's byte length — the kernel's own record, never the granting
    /// task's claim — to `len_out` on success.
    ///
    /// A class driver maps the URB data buffer its host-controller driver
    /// created and forwarded as a grant on the matched interface node, holding
    /// no DMA authority of its own.
    ///
    /// Mirrors [`tairix_rt::shm_map`].
    fn shm_map(&self, handle: u64, len_out: &mut u64) -> i64;

    /// Enumerate the device-resource grants the kernel minted for the
    /// calling task into `buf`, returning the total number of bytes written
    /// — consecutive [`tairix_abi::hwtree::GrantedResource`] records — when
    /// non-negative, else `-errno`. A buffer too small for the whole set
    /// fails closed.
    ///
    /// Mirrors [`tairix_rt::resource_grants`].
    fn resource_grants(&self, buf: &mut [u8]) -> i64;

    /// Bind interrupt `line` (an [`HwResourceKind::Irq`] grant the driver's
    /// matched node carried) to the calling task, returning the raw
    /// [`tairix_abi::IrqHandle`] (or `-errno`). Carries `CAP_IRQ_BIND`.
    ///
    /// [`HwResourceKind::Irq`]: tairix_abi::hwtree::HwResourceKind
    ///
    /// Mirrors [`tairix_rt::irq_bind`].
    fn irq_bind(&self, line: u32) -> i64;

    /// Park the calling task until the interrupt bound to `handle` fires (or
    /// `timeout_ns` elapses), returning `0` on a fire (or `-errno`). The
    /// kernel re-arms the bound line on the driver's behalf across the park
    /// (the driver holds no controller access).
    ///
    /// Mirrors [`tairix_rt::irq_wait`].
    fn irq_wait(&self, handle: u64, timeout_ns: u64) -> i64;

    /// Make a synchronous capability-checked call to the kernel-owned call
    /// endpoint `endpoint`: post `request`, block until the reply arrives,
    /// and copy it into `reply`, returning the number of reply bytes written
    /// (or `-errno`). The host uses this only to reach the firmware
    /// property-mailbox service ([`MailboxChannel`](tairix_abi::driver::MailboxChannel)
    /// over [`tairix_abi::mailbox_ipc::MAILBOX_ENDPOINT`]); the kernel gates
    /// the call by the endpoint's required send capability and copies both
    /// buffers through the validated boundary.
    ///
    /// Mirrors [`tairix_rt::ipc_call`].
    fn ipc_call(&self, endpoint: u64, request: &[u8], reply: &mut [u8]) -> i64;

    /// Publish a discovered child device `node` into the live hardware tree,
    /// returning `0` once published, else `-errno`. A user-space **bus**
    /// driver calls this for each device it enumerates so the device manager
    /// autoloads the matching driver in turn; the kernel admits the node only
    /// when every resource it requests is covered by one of the calling
    /// driver's own grants, so a child can never carry more authority than
    /// its emitter.
    ///
    /// Mirrors [`tairix_rt::hw_emit_node`].
    fn hw_emit_node(&self, node: &tairix_abi::HwNode) -> i64;

    /// Publish the fault-domain `health` of the interior node the calling
    /// driver owns into the live hardware tree, returning `0` once recorded,
    /// else `-errno`. A bus/hub/controller driver reports its own node's
    /// [`tairix_abi::blkio::FaultDomainState`] so the device manager reacts to
    /// *one* coherent fault-domain recovery episode across the subtree rather
    /// than N spurious child removals; the node stays present (a distinct
    /// signal from a surprise removal), and the kernel records only the
    /// calling driver's *own* matched node's health (no ambient authority).
    ///
    /// Mirrors [`tairix_rt::hw_node_health`].
    fn hw_node_health(&self, health: tairix_abi::blkio::FaultDomainState) -> i64;

    /// Allocate a message-signalled interrupt (MSI) vector for a PCI
    /// function, returning the [`tairix_abi::MsiAllocation`] the kernel
    /// minted (the virtual interrupt line plus the doorbell to program into
    /// the function's MSI capability), or the raw negative kernel result
    /// (`-errno`) on failure. Carries `CAP_IRQ_BIND`.
    ///
    /// Mirrors [`tairix_rt::msi_alloc`].
    fn msi_alloc(&self) -> Result<tairix_abi::MsiAllocation, i64>;
}

/// The production [`GrantSyscalls`]: forward to `tairix_rt`'s wrappers.
///
/// Zero-sized; a driver process constructs one and hands it to
/// [`RtDriverHost::new`](crate::RtDriverHost::new). It carries no state and no
/// authority — it only routes through the one shared syscall trap.
#[derive(Debug, Default, Copy, Clone)]
pub struct RtGrantSyscalls;

impl GrantSyscalls for RtGrantSyscalls {
    #[inline]
    fn mmio_map(&self, handle: u64, offset: u64, len: usize) -> i64 {
        tairix_rt::mmio_map(handle, offset, len)
    }

    #[inline]
    fn boot_facts(&self) -> Option<tairix_abi::BootFacts> {
        tairix_rt::boot_facts().ok()
    }

    #[inline]
    fn dma_alloc(&self, handle: u64, len: usize, device_out: &mut u64) -> i64 {
        tairix_rt::dma_alloc(handle, len, device_out)
    }

    #[inline]
    fn dma_free(&self, handle: u64, cpu_va: u64) -> i64 {
        tairix_rt::dma_free(handle, cpu_va)
    }

    #[inline]
    fn shm_map(&self, handle: u64, len_out: &mut u64) -> i64 {
        tairix_rt::shm_map(handle, len_out)
    }

    #[inline]
    fn resource_grants(&self, buf: &mut [u8]) -> i64 {
        tairix_rt::resource_grants(buf)
    }

    #[inline]
    fn irq_bind(&self, line: u32) -> i64 {
        tairix_rt::irq_bind(line)
    }

    #[inline]
    fn irq_wait(&self, handle: u64, timeout_ns: u64) -> i64 {
        tairix_rt::irq_wait(handle, timeout_ns)
    }

    #[inline]
    fn ipc_call(&self, endpoint: u64, request: &[u8], reply: &mut [u8]) -> i64 {
        // `tairix_rt::ipc_call` already clamps a non-negative count to the
        // reply buffer; surface the raw signed result for the host to decode.
        #[allow(clippy::cast_possible_wrap)]
        match tairix_rt::ipc_call(endpoint, request, reply) {
            Ok(count) => count as i64,
            Err(errno) => errno,
        }
    }

    #[inline]
    fn hw_emit_node(&self, node: &tairix_abi::HwNode) -> i64 {
        tairix_rt::hw_emit_node(node)
    }

    #[inline]
    fn hw_node_health(&self, health: tairix_abi::blkio::FaultDomainState) -> i64 {
        tairix_rt::hw_node_health(health)
    }

    #[inline]
    fn msi_alloc(&self) -> Result<tairix_abi::MsiAllocation, i64> {
        tairix_rt::msi_alloc()
    }
}
