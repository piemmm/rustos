//! The `Run` entry-point binary of the Broadcom Serial Controller I²C bus
//! driver, installed as a signed `/System/Drivers/` bundle and **autoloaded
//! into user space** by `devmgr` when a `brcm,bcm2835-i2c` node is discovered
//! (`plans/TIMESYNC.md` TS-4).
//!
//! The process owns the controller's register window and its interrupt line,
//! and serves **one transfer endpoint per child the device tree declared**.
//! Discovery split the two halves of each child's existence: this driver
//! received the *duty* — an endpoint id paired with that child's bus address —
//! while the chip's own driver received only the *authority*, an endpoint
//! grant naming the id. The address therefore lives only here, so a chip
//! driver cannot reach a neighbour however it is compromised, and the kernel
//! admits this bind only because the duty grant covers the id.
//!
//! It is a **pure-Rust** program linking the Rust userland runtime
//! `tairix-rt`, never the C ABI. `main` wires the real seams:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the register
//!   window, the interrupt line, and the duty list all come from the grants
//!   the kernel minted for the matched node.
//! * `host.bind_irq()` before the first transfer, so a transfer parks on the
//!   line rather than spinning — a bind that fails ends the driver rather
//!   than degrading to a poll loop.
//! * one `call_create` per duty, then a wait-set over every bound endpoint,
//!   so the process parks between requests.
//!
//! A bring-up failure exits with a reserved fail-closed code, leaving the bus
//! unserved rather than the machine wedged. On the host it is an inert stub
//! so `cargo build --workspace`, clippy, and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::driver::i2c::I2cAddress;
    use tairix_abi::driver::sole_register_window;
    use tairix_abi::hwtree::{HwResource, HwResourceKind};
    use tairix_abi::i2c_ipc;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{CapabilityId, MmioMapper};
    use tairix_caps::CapabilitySet;
    use tairix_drv_bus_i2c_bcm2835::{Bsc, BusWait};
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
    use tairix_rt::LogSink;
    use tairix_util::fmt::format_hex_u64;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 90;

    /// Exit code when the delivered grants do not name the controller's
    /// register window — an unbound or mis-provisioned node. A reserved,
    /// fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 91;

    /// Exit code when the register window could not be mapped or the
    /// controller would not come up. A reserved value.
    const EXIT_BRINGUP_FAILED: i32 = 92;

    /// Exit code when the granted interrupt line could not be bound. A
    /// transfer that cannot park would have to spin, which is forbidden, so
    /// the driver ends rather than degrade. A reserved value.
    const EXIT_NO_INTERRUPT: i32 = 93;

    /// Exit code when the node declared no children to serve, so there is
    /// nothing for this driver to do. A reserved value.
    const EXIT_NO_CHILDREN: i32 = 94;

    /// Exit code when the wait set could not be built or a park failed — a
    /// destroyed endpoint or a torn-down task, both terminal. Exiting
    /// fail-loud beats retrying a dead channel forever, which is a spin. A
    /// reserved value.
    const EXIT_SERVE_FAILED: i32 = 95;

    /// Bound on the number of children one bus node can declare, which is
    /// the ABI's own per-node resource bound: the duty list shares that
    /// array with the window and the line, so no bus can present more.
    const MAX_CHILDREN: usize = tairix_abi::HW_NODE_MAX_RESOURCES;

    /// Bound on the in-flight requests one child's endpoint queues. A chip
    /// driver blocks on its reply, so a small capacity absorbs only a
    /// re-submit racing the previous answer; it is a queue bound, not a
    /// hardware capacity.
    const ENDPOINT_CAPACITY: usize = 4;

    /// A wait with no deadline: the driver has nothing to do until a chip
    /// driver asks for a transfer.
    const WAIT_FOREVER_NS: u64 = u64::MAX;

    /// Audit range base for this driver's bind outcomes.
    const EVENT_CHILD_BOUND: EventId = EventId(24_100);
    /// A declared child whose duty this driver could not serve.
    const EVENT_CHILD_UNSERVED: EventId = EventId(24_101);

    /// The capability set the host re-checks before issuing a trap, plus the
    /// bind privilege each child endpoint needs. The kernel is the authority
    /// and re-checks every trap.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::IRQ_BIND);
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
        caps
    }

    /// The required-sender capability set of every child endpoint.
    ///
    /// `CAP_IPC_ENDPOINT` couples the per-endpoint grant to the call itself,
    /// so the kernel admits only the chip driver discovery handed that
    /// child's endpoint — no other principal can drive the part, whatever
    /// else it holds.
    fn endpoint_send_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::IPC_ENDPOINT);
        caps
    }

    /// The metal [`BusWait`]: park on the controller's bound interrupt line
    /// with the caller's remaining budget as the deadline, so a completion
    /// wakes the transfer early and a quiet bus costs no CPU.
    ///
    /// The host owns the bound handle, so the park is its one definition and
    /// the controller engine stays free of any syscall knowledge.
    struct IrqWait<'a>(&'a RtDriverHost<RtGrantSyscalls>);

    impl BusWait for IrqWait<'_> {
        fn now_ns(&self) -> u64 {
            tairix_rt::clock_get()
        }

        fn wait(&self, timeout_ns: u64) {
            // A refused park degrades to the caller's deadline check rather
            // than spinning; `main` already failed loud if the line could
            // not be bound at all.
            let _ = self.0.wait_irq(timeout_ns);
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the serve loop runs for the life of the
    /// driver process.
    fn main() -> i32 {
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        let Ok((base, len)) = sole_register_window(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let Ok(regs) = host.map_window(base, len) else {
            return EXIT_BRINGUP_FAILED;
        };
        if host.bind_irq().is_err() {
            return EXIT_NO_INTERRUPT;
        }
        let wait = IrqWait(&host);
        let Ok(bsc) = Bsc::new(&regs, &wait) else {
            return EXIT_BRINGUP_FAILED;
        };

        let mut children = [None; MAX_CHILDREN];
        let bound = bind_children(&host, &mut children);
        if bound == 0 {
            return EXIT_NO_CHILDREN;
        }
        serve(&bsc, &children)
    }

    /// Bind one restricted-sender endpoint per duty grant, returning how many
    /// came up.
    ///
    /// A duty whose address the tree spelled unusably, or whose endpoint the
    /// kernel refused, leaves that child unserved and is logged — its chip
    /// driver then fails closed rather than talking to the wrong part.
    fn bind_children(
        host: &RtDriverHost<RtGrantSyscalls>,
        children: &mut [Option<(u64, I2cAddress)>],
    ) -> usize {
        let mut bound = 0;
        let duties = host
            .resources()
            .filter(|r| r.kind() == Some(HwResourceKind::BusChild))
            .filter_map(HwResource::bus_child_pair);
        for (endpoint, raw_address) in duties {
            let Some(slot) = children.get_mut(bound) else {
                break;
            };
            let Ok(address) = I2cAddress::from_bus_address(raw_address) else {
                log_child(EVENT_CHILD_UNSERVED, Level::Warn, endpoint, raw_address);
                continue;
            };
            let send_caps = endpoint_send_caps();
            let recv_caps = CapabilitySet::empty();
            if tairix_rt::call_create(
                endpoint,
                &send_caps,
                &recv_caps,
                i2c_ipc::REQUEST_LEN,
                i2c_ipc::REPLY_LEN,
                ENDPOINT_CAPACITY,
            ) != 0
            {
                log_child(EVENT_CHILD_UNSERVED, Level::Warn, endpoint, raw_address);
                continue;
            }
            log_child(EVENT_CHILD_BOUND, Level::Info, endpoint, raw_address);
            *slot = Some((endpoint, address));
            bound += 1;
        }
        bound
    }

    /// Park on the wait set and serve every bound child's transfers for the
    /// life of the driver.
    fn serve(bsc: &Bsc<'_>, children: &[Option<(u64, I2cAddress)>]) -> i32 {
        let set = tairix_rt::waitset_create();
        if set <= 0 {
            return EXIT_SERVE_FAILED;
        }
        // `set > 0` checked above; it is a kernel-minted handle.
        #[allow(clippy::cast_sign_loss)]
        let set = set as u64;
        for (token, child) in children.iter().enumerate() {
            let Some((endpoint, _)) = child else { continue };
            let Ok(token) = u64::try_from(token) else {
                return EXIT_SERVE_FAILED;
            };
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Endpoint,
                *endpoint,
                token,
            ) != 0
            {
                return EXIT_SERVE_FAILED;
            }
        }

        let mut request = [0u8; i2c_ipc::REQUEST_LEN];
        let mut reply = [0u8; i2c_ipc::REPLY_LEN];
        loop {
            let mut token = 0u64;
            let woke = tairix_rt::waitset_wait(set, WAIT_FOREVER_NS, &mut token);
            if woke < 0 {
                return EXIT_SERVE_FAILED;
            }
            if woke != 0 {
                // A lapsed wake with no ready source; re-park.
                continue;
            }
            let Ok(index) = usize::try_from(token) else {
                continue;
            };
            let Some(Some((endpoint, address))) = children.get(index) else {
                continue;
            };
            let mut ticket = 0u64;
            match tairix_rt::call_recv(*endpoint, &mut request, &mut ticket) {
                Ok(n) => {
                    // The port is the bus driver's own view of which child
                    // this endpoint belongs to: the address comes from here,
                    // never from the frame.
                    let port = bsc.port(*address);
                    let len = i2c_ipc::serve_request(&port, &request[..n], &mut reply).unwrap_or(0);
                    let _ = tairix_rt::call_reply(*endpoint, ticket, &reply[..len]);
                }
                Err(_) => return EXIT_SERVE_FAILED,
            }
        }
    }

    /// Record one child's bind outcome with the endpoint and address that
    /// decided it.
    fn log_child(id: EventId, level: Level, endpoint: u64, address: u64) {
        let mut endpoint_buf = [0u8; 16];
        let mut address_buf = [0u8; 16];
        log(
            &LogSink,
            &Event {
                level,
                id,
                message: "i2c child",
                fields: &[
                    Field {
                        key: "endpoint",
                        value: FieldValue::Str(format_hex_u64(endpoint, &mut endpoint_buf)),
                    },
                    Field {
                        key: "address",
                        value: FieldValue::Str(format_hex_u64(address, &mut address_buf)),
                    },
                ],
            },
        );
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
