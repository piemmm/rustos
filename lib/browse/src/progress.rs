//! The progress + cancel model for a long file operation
//! (`plans/NEW-FILEMANAGER.md` `FM7b`).
//!
//! A recursive delete ([`DeleteWalk`](crate::DeleteWalk)) or copy
//! ([`CopyWalk`](crate::CopyWalk)) can touch many nodes, so the file manager
//! drives it interleaved with its event loop rather than in one blocking pass,
//! showing a progress trace and honouring a mid-run cancel. This module is the
//! pure display + cancel *state* of such a run; the app owns the driving walk
//! and the actual filesystem calls.
//!
//! The count is the honest rising figure the driving walk reports
//! ([`DeleteWalk::removed`](crate::DeleteWalk::removed) /
//! [`CopyWalk::copied`](crate::CopyWalk::copied)): the *total* is unknown until
//! the walk's own reads reveal it, so the model never fabricates a percentage —
//! the drawn trace is an indeterminate "working" bar captioned with the running
//! count. Cancel is *latched*, so a second press cannot un-cancel a run already
//! stopping.
//!
//! The model holds no authority and does no I/O — it is the display face of a
//! walk the app drives — so composing it grants nothing and the read-only
//! trusted picker (which never deletes or copies) never builds one.

use alloc::format;
use alloc::string::String;

/// Which long file operation a [`ProgressModel`] reports.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProgressOp {
    /// A recursive removal (driven by [`DeleteWalk`](crate::DeleteWalk)).
    Delete,
    /// A recursive copy or cross-volume move (driven by
    /// [`CopyWalk`](crate::CopyWalk)).
    Copy,
    /// A move to Trash: a recoverable delete carried out as same-volume
    /// renames into the user's Trash directory (`plans/NEW-FILEMANAGER.md`
    /// `FM10`), one item per step.
    Trash,
}

/// The running display + cancel state of a long file operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressModel {
    op: ProgressOp,
    done: usize,
    cancel_requested: bool,
}

impl ProgressModel {
    /// A fresh progress model for `op`, with nothing done yet and no cancel
    /// requested.
    #[must_use]
    pub const fn new(op: ProgressOp) -> Self {
        Self {
            op,
            done: 0,
            cancel_requested: false,
        }
    }

    /// Which operation this reports.
    #[must_use]
    pub const fn op(&self) -> ProgressOp {
        self.op
    }

    /// The honest count of nodes processed so far (the driving walk's own
    /// rising figure).
    #[must_use]
    pub const fn done(&self) -> usize {
        self.done
    }

    /// Update the processed-node count from the driving walk. The caller passes
    /// the walk's own count, so the caption can never claim more than actually
    /// happened.
    pub fn set_done(&mut self, done: usize) {
        self.done = done;
    }

    /// Latch a user cancel request. The app stops the walk at the next
    /// step/chunk boundary (never mid-operation on a single node), and the
    /// latch means a second press cannot revert a run already stopping.
    pub fn request_cancel(&mut self) {
        self.cancel_requested = true;
    }

    /// Whether the user has asked to cancel this run.
    #[must_use]
    pub const fn is_cancel_requested(&self) -> bool {
        self.cancel_requested
    }

    /// The panel title: the operation in progress, or "Cancelling…" once a
    /// cancel has been latched (so the user sees the request took effect while
    /// the current step finishes).
    #[must_use]
    pub const fn title(&self) -> &'static str {
        if self.cancel_requested {
            "Cancelling\u{2026}"
        } else {
            match self.op {
                ProgressOp::Delete => "Deleting\u{2026}",
                ProgressOp::Copy => "Copying\u{2026}",
                ProgressOp::Trash => "Moving to Trash\u{2026}",
            }
        }
    }

    /// The running caption: the honest count of nodes processed so far with the
    /// operation's verb (`"3 items removed"`, `"1 item copied"`) — never a
    /// fabricated percentage, since the total is unknown until the walk reveals
    /// it.
    #[must_use]
    pub fn status_line(&self) -> String {
        let verb = match self.op {
            ProgressOp::Delete => "removed",
            ProgressOp::Copy => "copied",
            ProgressOp::Trash => "moved to Trash",
        };
        let noun = if self.done == 1 { "item" } else { "items" };
        format!("{} {noun} {verb}", self.done)
    }
}
