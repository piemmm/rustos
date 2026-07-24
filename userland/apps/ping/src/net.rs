//! The network + clock seam through which `ping` sends echo requests and
//! receives replies.
//!
//! The engine ([`crate::client`]) is pure and host-testable: it never names
//! a syscall. All contact with the outside world — the monotonic clock, the
//! ICMP echo socket, and the wait/park between sends — goes through this
//! object-safe [`PingIo`] trait. The production implementation
//! (`src/run.rs`) drives it with the `tairix-rt` ICMP-echo socket wrappers
//! and clock/wait-set syscalls; host tests drive it with an in-memory fake.

use alloc::vec::Vec;

use tairix_abi::net_ipc::NetAddrFamily;
use tairix_abi::Errno;

/// One received echo reply, owned so the engine can verify it after the
/// borrow of the receive buffer ends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoReply {
    /// The echoed sequence number.
    pub seq: u16,
    /// The source family.
    pub family: NetAddrFamily,
    /// The source address bytes (IPv4 uses the first four).
    pub addr: [u8; 16],
    /// The echoed payload.
    pub payload: Vec<u8>,
}

/// The clock, echo socket, and wait/park the ping engine drives.
pub trait PingIo {
    /// The current monotonic time in nanoseconds.
    fn now(&self) -> u64;

    /// Send one echo request bearing `seq` and `payload` to the connected
    /// peer.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the send raised (e.g. [`Errno::NetworkUnreachable`]
    /// while the interface is still coming up).
    fn send(&mut self, seq: u16, payload: &[u8]) -> Result<(), Errno>;

    /// Wait for the next echo reply, giving the CPU up until one arrives or
    /// the absolute `deadline_ns` passes (never a busy spin). Returns
    /// [`Some`] with a reply, or [`None`] when the deadline passed with no
    /// reply.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] a fatal receive error raised (a transient empty
    /// mailbox is not an error — it parks and retries until the deadline).
    fn recv(&mut self, deadline_ns: u64) -> Result<Option<EchoReply>, Errno>;

    /// Park until the absolute `deadline_ns` (the inter-request spacing),
    /// giving the CPU up rather than spinning.
    fn sleep_until(&mut self, deadline_ns: u64);
}
