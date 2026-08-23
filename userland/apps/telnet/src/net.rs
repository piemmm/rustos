//! The seam through which the session touches the outside world.
//!
//! The engine in [`crate::client`] is single-threaded and pure: it asks for the
//! next event and reacts. That is possible even though telnet must relay both
//! directions at once because the *seam* owns the multiplexing — the production
//! implementation (`src/run.rs`) parks one wait-set over both the stack's
//! socket-delivery port and the port its keyboard-reader thread posts to, so
//! neither side is ever polled and the engine sees one ordered event stream.
//!
//! Host tests drive the same engine with a scripted in-memory [`TelnetIo`], so
//! the tested code and the shipped code are the same code.

use alloc::vec::Vec;

use tairix_abi::net_ipc::NetAddrFamily;
use tairix_abi::{Errno, InputMode, TerminalSize};

/// A resolved remote endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Endpoint {
    /// The address family the connection will use.
    pub family: NetAddrFamily,
    /// The address, IPv4 in the first four octets.
    pub addr: [u8; 16],
    /// The TCP port.
    pub port: u16,
}

/// Why a connection ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseReason {
    /// The peer closed its side of the connection orderly.
    PeerClosed,
    /// The connection was reset or otherwise aborted.
    Reset,
    /// The local side closed it (the `close`/`quit` commands).
    Local,
}

/// One event the session reacts to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IoEvent {
    /// Bytes arrived from the network, verbatim and un-interpreted.
    Network(Vec<u8>),
    /// Bytes arrived from the local terminal, verbatim.
    Keyboard(Vec<u8>),
    /// The local terminal reached end of input; nothing more will be typed.
    KeyboardClosed,
    /// The connection ended.
    Closed(CloseReason),
}

/// The connection, resolver and terminal the session drives.
pub trait TelnetIo {
    /// Resolve `host` to an endpoint on `port`, restricted to `family` when the
    /// command line named one. A literal address resolves without a query.
    ///
    /// Returns [`None`] when the host has no address of the wanted family — the
    /// caller reports it naming the host, so no error type is needed here.
    fn resolve(&mut self, host: &str, port: u16, family: Option<NetAddrFamily>)
        -> Option<Endpoint>;

    /// Open a connection to `endpoint`, replacing any current one.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the open or connect raised — [`Errno::PermissionDenied`]
    /// without `CAP_NET`, [`Errno::NetworkUnreachable`] with no route.
    fn connect(&mut self, endpoint: Endpoint) -> Result<(), Errno>;

    /// Whether a connection is currently open.
    fn connected(&self) -> bool;

    /// Block until the next event, giving the CPU up meanwhile (never a spin).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] a fatal wait or receive raised. A transient empty mailbox
    /// is not an error — the implementation parks and retries.
    fn next_event(&mut self) -> Result<IoEvent, Errno>;

    /// Transmit `bytes` on the connection, retrying a momentarily-full send
    /// buffer until every byte is accepted.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the send raised — [`Errno::NotConnected`] once the peer
    /// has gone, or a transport failure.
    fn send(&mut self, bytes: &[u8]) -> Result<(), Errno>;

    /// The terminal's current character grid, or [`None`] when the console
    /// cannot attest one (a serial line, whose far-end terminal the kernel
    /// cannot know).
    fn terminal_size(&mut self) -> Option<TerminalSize>;

    /// Select the local read line discipline. The session takes
    /// [`InputMode::Raw`] for the relay and restores [`InputMode::Cooked`]
    /// before it ends, so the next program on this console sees the
    /// interactive default.
    fn set_input_mode(&mut self, mode: InputMode);

    /// Stop this process until it is continued (the `z` command).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the request raised; an environment with no job control
    /// refuses and the session continues.
    fn suspend(&mut self) -> Result<(), Errno>;

    /// Send the connection's FIN, keeping it readable (the `send eof` path for
    /// a server that expects a half-close).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the request raised.
    fn shutdown_write(&mut self) -> Result<(), Errno>;

    /// Release the connection.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the close raised.
    fn close(&mut self) -> Result<(), Errno>;
}
