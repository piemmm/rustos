//! [`RtBlkCall`]: the production blkio transport.
//!
//! The bounded, capability-checked async submit/reap seam on a granted
//! block-service endpoint (`plans/FIX-IO.md` IO1), shared by every
//! [`crate::RemoteBlock`] consumer so the volume-manager probe and the RAID
//! array composer issue the identical wire discipline. Each request is
//! `call_post`ed with the caller's per-request deadline, the reply awaited
//! on a `CallReply` wait-set, and reaped with `call_reap`, so a wedged
//! device fails this transfer closed at its deadline instead of parking the
//! caller forever. The deadline is the caller's, derived from the device's
//! own declared class, so this transport carries no deadline policy of its
//! own. The serving driver fills the shared window during the call, so the
//! window parameter is untouched here.

use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::Errno;

use crate::client::BlkCall;

/// Recover an [`Errno`] from a raw negative kernel result (`-errno`),
/// failing closed to [`Errno::NotFound`] for a code this vocabulary does not
/// recognise — never a fabricated success.
fn errno_from(neg: i64) -> Errno {
    Errno::from_i32(i32::try_from(-neg).unwrap_or(0)).unwrap_or(Errno::NotFound)
}

/// The production blkio transport: one call endpoint plus the wait-set
/// multiplexing its reply completions.
pub struct RtBlkCall {
    endpoint: u64,
    /// Created lazily on first use and `0` until then. One device needs
    /// only one member, but using the wait-set seam even here keeps the
    /// single transport shape a multi-device consumer also uses.
    waitset: u64,
}

impl RtBlkCall {
    /// A fresh transport for `endpoint`, its wait-set unset until the first
    /// [`BlkCall::call`].
    #[must_use]
    pub fn new(endpoint: u64) -> Self {
        Self {
            endpoint,
            waitset: 0,
        }
    }

    /// Mint the reply wait-set and register this endpoint's `CallReply`
    /// member, once. The wait-set is reclaimed by the kernel when the
    /// owning run-to-completion program exits, so it needs no explicit
    /// teardown.
    fn ensure_waitset(&mut self) -> Result<u64, Errno> {
        if self.waitset == 0 {
            let set = tairix_rt::waitset_create();
            if set < 0 {
                return Err(errno_from(set));
            }
            #[allow(clippy::cast_sign_loss)] // `set >= 0` is the minted handle, checked above.
            let set = set as u64;
            let ctl = tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::CallReply,
                self.endpoint,
                0,
            );
            if ctl < 0 {
                return Err(errno_from(ctl));
            }
            self.waitset = set;
        }
        Ok(self.waitset)
    }
}

impl BlkCall for RtBlkCall {
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        _window: &mut [u8],
        deadline_ns: u64,
    ) -> Result<usize, Errno> {
        let set = self.ensure_waitset()?;
        let ticket =
            tairix_rt::call_post(self.endpoint, request, deadline_ns).map_err(errno_from)?;
        loop {
            match tairix_rt::call_reap(self.endpoint, ticket, reply) {
                Ok(len) => return Ok(len),
                Err(neg) => {
                    let err = errno_from(neg);
                    // Not ready yet: park on the reply wait-set until the
                    // reply lands or the per-request deadline elapses (which
                    // makes the member ready and the next reap `TimedOut`),
                    // never a busy poll. Every other outcome — a timeout, a
                    // vanished endpoint — fails closed.
                    if err == Errno::WouldBlock {
                        let mut token = 0u64;
                        let _ = tairix_rt::waitset_wait(set, deadline_ns, &mut token);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }
}
