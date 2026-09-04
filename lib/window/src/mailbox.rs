//! The application's own event mailbox: the authenticated drain half of its
//! event stream.
//!
//! Every windowed app binds one endpoint for the whole process and reads the
//! session's deliveries from it. The mailbox is open to any sender capable of
//! naming the endpoint, so the kernel-attested origin of each frame — not its
//! content — is the authentication: a frame of the wrong length, or from any
//! sender other than the session the create reply named, is dropped rather
//! than delivered.
//!
//! That rule lives here so it has one definition rather than one per app.

use tairix_abi::window_ipc::WindowEvent;
use tairix_abi::{Errno, Origin, ProcId, ORIGIN_WIRE_LEN};

use crate::client::EventDrain;

/// The app's own event mailbox, accepting only the desktop session's frames.
pub struct EventMailbox {
    endpoint: u64,
    server: ProcId,
}

impl EventMailbox {
    /// The mailbox bound at `endpoint`, accepting only frames the kernel
    /// attests came from `server` — the session identity the create reply
    /// named.
    #[must_use]
    pub const fn new(endpoint: u64, server: ProcId) -> Self {
        Self { endpoint, server }
    }
}

impl EventDrain for EventMailbox {
    fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
        loop {
            let mut sender = [0u8; ORIGIN_WIRE_LEN];
            match tairix_rt::ipc_recv(self.endpoint, event, &mut sender) {
                Ok(len) if accepted(len, &sender, self.server) => return Ok(true),
                Ok(_) => {}
                Err(err) if Errno::from_syscall(err) == Errno::WouldBlock => return Ok(false),
                Err(err) => return Err(Errno::from_syscall(err)),
            }
        }
    }
}

/// Whether a received frame is a genuine event from the desktop session:
/// exactly one [`WindowEvent`] wide and from the attested `server` origin.
fn accepted(len: usize, sender: &[u8; ORIGIN_WIRE_LEN], server: ProcId) -> bool {
    len == WindowEvent::WIRE_LEN
        && Origin::from_bytes(sender).is_ok_and(|origin| origin.proc_id() == server)
}
