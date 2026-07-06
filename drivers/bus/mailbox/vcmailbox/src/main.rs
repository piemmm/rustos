//! The `Run` entry-point binary of the `VideoCore` firmware property-mailbox
//! **service driver**, installed as a signed `/System/Drivers/` bundle and
//! **autoloaded into user space** by `devmgr` when the BCM2711 mailbox node is
//! discovered (`plans/PI.md` P10 D3).
//!
//! This moves the `VideoCore` mailbox out of the kernel (the floor stays
//! storage-only) into a user-space service: it owns the
//! discovered doorbell MMIO window and a DMA-carved property buffer, builds the
//! BCM2711 `VideoCore` transport (`lib/vcmailbox::MmioMailbox`), and answers
//! *synchronous* property exchanges from other user-space drivers — the VL805
//! USB firmware reload (`drivers/bus/usb/vl805`) — over the well-known
//! `rustos_abi::mailbox_ipc::MAILBOX_ENDPOINT` call endpoint.
//!
//! The hardware mechanism (doorbell registers, DMA buffer, bus-address
//! translation, cache coherency) lives entirely behind the transport; the
//! service keeps no protocol logic of its own — it decodes each request,
//! runs the exchange, and frames the reply through
//! `rustos_abi::mailbox_ipc::serve_request`. A caller's
//! authority is enforced kernel-side by the endpoint's `CAP_MAILBOX` send gate: the service serves whoever the kernel admitted
//! and validates nothing about the caller itself.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt` (`_start`, the stack canary, the panic handler,
//! and the `call_create` / `call_recv` / `call_reply` / `yield` syscall
//! wrappers), never the C ABI. `main` wires the real seams:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the host learns
//!   its kernel-issued grants (the doorbell window + a DMA constraint) and
//!   maps/carves them. Every capability and bound is re-checked kernel-side; the host adds no authority. The kernel carves
//!   coherent DMA, so no architecture-specific cache shim is supplied
//!   (`coherency = None`, keeping the program free of arch code).
//! * `sole_register_window` over the delivered grants: the doorbell window
//!   `(base, len)` comes from the grants, never a build-time board constant.
//! * `host.alloc_dma_zeroed` carves the `PROPERTY_LEN_BYTES` property buffer;
//!   its device-visible base is the firmware's bus address for the buffer.
//! * `MmioMailbox::new` over the doorbell window and the property buffer, then
//!   `call_create` to bind the restricted-sender endpoint, then the serve loop.
//!
//! After bring-up `main` serves forever: it blocks in `call_recv`, transforms
//! each request, and answers with `call_reply` (a genuine
//! block, never a busy spin). A bring-up failure exits with a reserved
//! fail-closed code, leaving the system without a mailbox service rather than
//! wedged; the spawning supervisor decides whether to
//! relaunch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use core::cell::RefCell;
    use core::ptr::NonNull;

    use rustos_abi::driver::dma::DmaHost;
    use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
    use rustos_abi::driver::sole_register_window;
    use rustos_abi::mailbox_ipc::{self, MAILBOX_ENDPOINT};
    use rustos_abi::{CapabilityId, DriverError, MmioMapper, RegisterWindow};
    use rustos_caps::CapabilitySet;
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_vcmailbox::{
        MailboxError, MailboxTransport, MmioMailbox, DEFAULT_POLL_BUDGET, PROPERTY_LEN_BYTES,
    };

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the single doorbell
    /// register window this service needs — an unbound or mis-provisioned
    /// node. A reserved, fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the mailbox transport could not be built (the doorbell
    /// window could not be mapped, the DMA property buffer could not be
    /// carved, or its geometry is unusable). A reserved, fail-closed value.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Exit code when the call endpoint could not be created (the id is
    /// already bound, or the service lacks `CAP_IPC_BIND_PRIVILEGED` for a
    /// restricted-sender endpoint). A reserved, fail-closed value.
    const EXIT_ENDPOINT_FAILED: i32 = 83;

    /// Exit code when the serve loop's `call_recv` failed — a destroyed
    /// endpoint or a torn-down task, both terminal. Exiting fail-loud beats
    /// yield-retrying a dead channel forever, which is a busy spin; the
    /// spawning supervisor decides whether to relaunch. A reserved value.
    const EXIT_SERVE_FAILED: i32 = 84;

    /// Property-channel poll budget for a single exchange. The main consumer
    /// is the VL805 firmware reload, which the kernel scaffold allowed a
    /// generous budget; mirror that headroom so a slow firmware reply is not
    /// spuriously timed out (sized by reasoning, not
    /// guesswork).
    const POLL_BUDGET: u32 = 10 * DEFAULT_POLL_BUDGET;

    /// Bound on the number of in-flight requests the endpoint queues. The
    /// service answers each request before receiving the next, so a small
    /// capacity suffices; it is a queue bound, not a hardware capacity.
    const ENDPOINT_CAPACITY: usize = 4;

    /// The capability set the host re-checks before issuing a `mmio_map` /
    /// `dma_alloc` trap, plus the bind privilege the service needs to create a
    /// restricted-sender endpoint. The kernel re-checks every trap regardless.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::MEM_DMA);
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
        caps
    }

    /// The required-sender capability set of the served endpoint: a caller
    /// must hold `CAP_MAILBOX` to post a request.
    fn endpoint_send_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MAILBOX);
        caps
    }

    /// Adapts the `lib/vcmailbox` `&mut self` [`MailboxTransport`] onto the
    /// `&self` [`MailboxChannel`] the wire-level server transform consumes.
    ///
    /// Sound because the service is the transport's only, single-threaded
    /// caller (the service serialises access). A transport
    /// [`MailboxError`] is mapped to the board-neutral [`DriverError`] the
    /// seam reports, which [`mailbox_ipc::serve_request`] then frames as an
    /// in-band error reply (fail closed).
    struct ServiceChannel {
        mailbox: RefCell<MmioMailbox>,
    }

    impl MailboxChannel for ServiceChannel {
        fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
            self.mailbox
                .borrow_mut()
                .exchange(message)
                .map_err(MailboxError::as_driver_error)
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the serve loop runs for the life of the
    /// service process.
    fn main() -> i32 {
        // Build the host from the grants the kernel minted. The kernel carves
        // coherent DMA, so no architecture-specific cache shim is supplied.
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // Resolve and map the single granted doorbell window.
        let Ok((base, len)) = sole_register_window(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let Ok(regs) = host.map_window(base, len) else {
            return EXIT_BRINGUP_FAILED;
        };
        // Carve the DMA-visible property buffer; its device-visible base is
        // the firmware's bus address for the buffer.
        let Ok(mut slab) = host.alloc_dma_zeroed(PROPERTY_LEN_BYTES) else {
            return EXIT_BRINGUP_FAILED;
        };
        let buffer_phys = slab.phys();
        let Ok(buffer_bus) = u32::try_from(buffer_phys) else {
            // The VideoCore property channel addresses the buffer with a
            // 32-bit bus address; a carve above 4 GiB cannot be reached
            // (fail closed, never a truncated alias).
            return EXIT_BRINGUP_FAILED;
        };
        let bytes = slab.as_bytes_mut();
        let buffer_len = bytes.len();
        let Some(buffer_ptr) = NonNull::new(bytes.as_mut_ptr()) else {
            return EXIT_BRINGUP_FAILED;
        };
        // SAFETY: `buffer_ptr`/`buffer_len` name the DMA slab `host` just
        // carved for this process; the slab is owned by `slab` (a local that
        // lives for the whole serve loop, since `main` never returns), so the
        // window never outlives its backing. The `MmioMailbox` bounds every
        // access to the property message. `buffer_phys` is the slab's
        // device-visible base, the correct `phys` for the window.
        let buffer = unsafe { RegisterWindow::from_mapping(buffer_phys, buffer_ptr, buffer_len) };
        let Ok(mailbox) = MmioMailbox::new(regs, buffer, buffer_bus, POLL_BUDGET) else {
            return EXIT_BRINGUP_FAILED;
        };
        let channel = ServiceChannel {
            mailbox: RefCell::new(mailbox),
        };

        // Create the restricted-sender call endpoint other drivers reach the
        // service through. A non-zero result is a fail-closed refusal (the id
        // is already bound, or the bind privilege is missing).
        let send_caps = endpoint_send_caps();
        let recv_caps = CapabilitySet::empty();
        if rustos_rt::call_create(
            MAILBOX_ENDPOINT,
            &send_caps,
            &recv_caps,
            mailbox_ipc::REQUEST_LEN,
            mailbox_ipc::REPLY_LEN,
            ENDPOINT_CAPACITY,
        ) != 0
        {
            return EXIT_ENDPOINT_FAILED;
        }

        serve(&channel)
        // `slab` is intentionally kept alive until here (the serve loop runs
        // for the life of the service); its drop after `serve` returns is
        // what releases the buffer the mailbox referenced.
    }

    /// The serve loop: block in `call_recv` (a real park between requests),
    /// transform the request through the mailbox transport, and answer with
    /// `call_reply`.
    ///
    /// A `call_recv` error is terminal — the endpoint is sized so an
    /// oversize request is refused at send time, leaving only a destroyed
    /// endpoint or a torn-down task — so it ends the service fail-loud
    /// (the supervisor decides whether to relaunch) rather than yield-retry
    /// a dead channel forever, which is a busy spin. A reply that fails to
    /// encode is dropped to a zero-length reply, which the client decodes
    /// as a fail-closed truncation.
    fn serve(channel: &ServiceChannel) -> i32 {
        let mut request = [0u8; mailbox_ipc::REQUEST_LEN];
        let mut reply = [0u8; mailbox_ipc::REPLY_LEN];
        loop {
            let mut ticket = 0u64;
            match rustos_rt::call_recv(MAILBOX_ENDPOINT, &mut request, &mut ticket) {
                Ok(n) => {
                    let len =
                        mailbox_ipc::serve_request(channel, &request[..n], &mut reply).unwrap_or(0);
                    let _ = rustos_rt::call_reply(MAILBOX_ENDPOINT, ticket, &reply[..len]);
                }
                Err(_) => return EXIT_SERVE_FAILED,
            }
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
