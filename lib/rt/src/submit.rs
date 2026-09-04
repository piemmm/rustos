//! Submitting a figure about this process to a service without ever waiting
//! for the answer.
//!
//! A process states things about itself that only it can see — what its
//! reclaimable caches hold ([`cachereport`](crate::cachereport)), what the
//! desktop's frames cost — to the System Information service, which retains
//! them for a reader to query. The statement is a submission: the sender wants
//! it recorded, not answered, and no caller's next action depends on the
//! reply.
//!
//! `ipc_call` is the wrong shape for that. It parks the calling task off the
//! run queue until the service replies, so the sender pays a full cross-process
//! round trip — two scheduling handoffs plus the service's own turn — for a
//! statistic. On an interactive loop that is a stall the user sees: the desktop
//! compositor made two such calls a second per publisher, and a hover sweep in
//! the aarch64 QEMU vertical measured them at 11–39 ms each, several whole
//! frames apiece, with every application blocked in a window call behind it.
//!
//! [`Submission`] is the shape that fits. The request is *posted* (`call_post`,
//! which queues it and returns), the loop carries on drawing, and the status is
//! reaped without blocking (`call_reap`) the next time the publisher passes by.
//! One submission is in flight at a time — a restatement of a figure has
//! nothing to say until the last one has landed — and each carries a deadline,
//! so a wedged or absent service costs one abandoned ticket rather than a
//! blocked loop.

use tairix_abi::sysinfo::{decode_reply, SYSINFO_ENDPOINT, SYSINFO_REPLY_STATUS_LEN};
use tairix_abi::Errno;

/// One outstanding submission to the System Information service: posted,
/// never awaited.
///
/// Held by the publisher whose figures it carries. A publisher drives it in
/// the order [`settle`](Self::settle) then [`post`](Self::post): the answer to
/// the last submission decides whether the figure it carried is what the
/// service now holds, and only then is a fresh one worth making.
#[derive(Debug)]
pub struct Submission {
    /// How long the service has to answer before the ticket is abandoned.
    /// A submission nobody answers must not block the next one for ever.
    deadline_ns: u64,
    /// The ticket of the submission in flight, or `None` when none is.
    ticket: Option<u64>,
}

impl Submission {
    /// A channel whose submissions are abandoned after `deadline_ns`, with
    /// nothing yet in flight.
    ///
    /// Pass the publisher's own rate-limit interval as the deadline: a
    /// submission still unanswered when the next one is due is one the service
    /// was never going to answer in time to matter.
    #[must_use]
    pub const fn new(deadline_ns: u64) -> Self {
        Self {
            deadline_ns,
            ticket: None,
        }
    }

    /// Hand `request` to the service.
    ///
    /// The request is queued on the endpoint and this returns at once; the
    /// service's answer is collected by a later [`settle`](Self::settle).
    ///
    /// # Errors
    ///
    /// [`Errno::WouldBlock`] when a submission is already in flight — the
    /// caller treats that as the unsuccessful attempt it is and restates the
    /// figure on its next turn — or whatever the post itself refused (an absent
    /// service is `NotFound`, an over-capacity endpoint `LengthOutOfRange`).
    pub fn post(&mut self, request: &[u8]) -> Result<(), Errno> {
        if self.ticket.is_some() {
            return Err(Errno::WouldBlock);
        }
        let ticket = crate::call_post(SYSINFO_ENDPOINT, request, self.deadline_ns)
            .map_err(Errno::from_syscall)?;
        self.ticket = Some(ticket);
        Ok(())
    }

    /// The service's verdict on the submission in flight, or `None` while it
    /// is still unanswered and when there is none.
    ///
    /// Never blocks: an unanswered submission stays in flight and is asked
    /// about again on the caller's next turn. A deadline that has elapsed
    /// settles as [`Errno::TimedOut`] and retires the ticket, so a service that
    /// stopped answering costs one restated figure per interval and nothing
    /// else.
    pub fn settle(&mut self) -> Option<Result<(), Errno>> {
        let ticket = self.ticket?;
        let mut reply = [0u8; SYSINFO_REPLY_STATUS_LEN];
        match crate::call_reap(SYSINFO_ENDPOINT, ticket, &mut reply) {
            Err(raw) if Errno::from_syscall(raw) == Errno::WouldBlock => None,
            Err(raw) => {
                self.ticket = None;
                Some(Err(Errno::from_syscall(raw)))
            }
            Ok(len) => {
                self.ticket = None;
                Some(decode_reply(&reply[..len]).map(|_| ()))
            }
        }
    }

    /// Withdraw the submission in flight, if any, so nothing it carries can
    /// land after whatever the caller does next.
    ///
    /// This is what keeps a withdrawal ordered: a report posted moments before
    /// would otherwise be recorded *after* it and resurrect the rows it
    /// removed.
    pub fn abandon(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            let _ = crate::call_cancel(SYSINFO_ENDPOINT, ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use tairix_abi::{SyscallNumber, SYSCALL_MAX_ARGS};
    use tairix_abi_trap::seam;

    use super::{Errno, Submission};

    const DEADLINE_NS: u64 = 250_000_000;

    /// Run `call` with the trap seam armed to answer `ret`, handing back the
    /// `(number, args)` it recorded.
    fn capture(ret: u64, call: impl FnOnce()) -> (u64, [u64; SYSCALL_MAX_ARGS]) {
        seam::arm(ret);
        call();
        seam::last_call().expect("exactly one trap")
    }

    /// Arm the seam and assert `call` issued no trap at all.
    fn no_trap(call: impl FnOnce()) {
        seam::arm(0);
        call();
        assert!(seam::last_call().is_none(), "no trap may be issued");
    }

    /// The negative register the kernel encodes `errno` as.
    fn refusal(errno: Errno) -> u64 {
        u64::from_ne_bytes((-i64::from(errno.as_i32())).to_ne_bytes())
    }

    fn channel() -> Submission {
        Submission::new(DEADLINE_NS)
    }

    #[test]
    fn a_post_hands_the_request_to_the_endpoint_with_its_deadline() {
        let mut chan = channel();
        let request = [1u8, 2, 3];
        let (number, args) = capture(0, || {
            chan.post(&request).expect("armed post is taken");
        });
        assert_eq!(number, u64::from(SyscallNumber::CALL_POST.as_u16()));
        assert_eq!(args[0], super::SYSINFO_ENDPOINT);
        assert_eq!(args[2], request.len() as u64);
        assert_ne!(
            args[3], 0,
            "the kernel is given somewhere to write the ticket"
        );
        assert_eq!(args[4], DEADLINE_NS);
    }

    #[test]
    fn a_refused_post_leaves_nothing_in_flight() {
        let mut chan = channel();
        seam::arm(refusal(Errno::NotFound));
        assert_eq!(chan.post(b"x"), Err(Errno::NotFound));
        // Nothing outstanding, so the next attempt reaches the endpoint
        // rather than refusing itself.
        let (number, _) = capture(0, || {
            chan.post(b"x").expect("armed post is taken");
        });
        assert_eq!(number, u64::from(SyscallNumber::CALL_POST.as_u16()));
    }

    #[test]
    fn a_second_post_while_one_is_in_flight_is_refused_without_a_trap() {
        let mut chan = channel();
        seam::arm(0);
        chan.post(b"x").expect("armed post is taken");
        no_trap(|| {
            assert_eq!(chan.post(b"y"), Err(Errno::WouldBlock));
        });
    }

    #[test]
    fn settling_nothing_is_nothing_and_costs_no_trap() {
        let mut chan = channel();
        no_trap(|| {
            assert!(chan.settle().is_none());
        });
    }

    #[test]
    fn an_unanswered_submission_stays_in_flight() {
        let mut chan = channel();
        seam::arm(0);
        chan.post(b"x").expect("armed post is taken");

        seam::arm(refusal(Errno::WouldBlock));
        assert!(chan.settle().is_none(), "still unanswered");
        // Still outstanding: a fresh post is refused rather than replacing it.
        no_trap(|| {
            assert_eq!(chan.post(b"y"), Err(Errno::WouldBlock));
        });
    }

    #[test]
    fn an_answered_submission_settles_and_frees_the_channel() {
        let mut chan = channel();
        seam::arm(0);
        chan.post(b"x").expect("armed post is taken");

        // A status word of zero is the service's acceptance; the host seam
        // leaves the reply buffer zeroed, which is exactly that frame.
        let (number, args) = capture(super::SYSINFO_REPLY_STATUS_LEN as u64, || {
            assert_eq!(chan.settle(), Some(Ok(())));
        });
        assert_eq!(number, u64::from(SyscallNumber::CALL_REAP.as_u16()));
        assert_eq!(args[0], super::SYSINFO_ENDPOINT);
        assert_eq!(
            args[3],
            super::SYSINFO_REPLY_STATUS_LEN as u64,
            "a submission is answered by a status word and nothing more"
        );

        let (number, _) = capture(0, || {
            chan.post(b"y").expect("the channel is free again");
        });
        assert_eq!(number, u64::from(SyscallNumber::CALL_POST.as_u16()));
    }

    #[test]
    fn an_elapsed_deadline_settles_as_a_timeout_and_frees_the_channel() {
        let mut chan = channel();
        seam::arm(0);
        chan.post(b"x").expect("armed post is taken");

        seam::arm(refusal(Errno::TimedOut));
        assert_eq!(chan.settle(), Some(Err(Errno::TimedOut)));
        // The wedged service cost one abandoned ticket, not a blocked caller.
        let (number, _) = capture(0, || {
            chan.post(b"y").expect("the channel is free again");
        });
        assert_eq!(number, u64::from(SyscallNumber::CALL_POST.as_u16()));
    }

    #[test]
    fn abandoning_withdraws_the_submission_in_flight() {
        let mut chan = channel();
        seam::arm(0);
        chan.post(b"x").expect("armed post is taken");

        let (number, args) = capture(0, || chan.abandon());
        assert_eq!(number, u64::from(SyscallNumber::CALL_CANCEL.as_u16()));
        assert_eq!(args[0], super::SYSINFO_ENDPOINT);

        no_trap(|| {
            assert!(chan.settle().is_none(), "nothing is outstanding");
        });
    }

    #[test]
    fn abandoning_nothing_costs_no_trap() {
        let mut chan = channel();
        no_trap(|| chan.abandon());
    }
}
