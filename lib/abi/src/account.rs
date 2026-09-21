//! The field bounds of one account record, shared by every layer that
//! stores, carries, or renders one.
//!
//! The `users-v1` / `groups-v1` databases enforce these bounds on what
//! may be stored; the fixed-width [`sysinfo`](crate::sysinfo) directory
//! and account frames size their inline buffers from the same figures.
//! They are equal by definition — a record the database accepts must fit
//! the frame that reports it — so they are defined here once and imported
//! (`tairix_users` re-exports them), never restated per layer.
//!
//! These are **validation bounds, not capacities**: they exist so a
//! hostile or corrupt database line cannot force an unbounded name, path,
//! or group set through a decoder. Raising one is a reviewed change to
//! this file, never a per-call workaround.

/// Longest account name, in bytes.
///
/// The long-standing Unix `LOGIN_NAME_MAX` ceiling.
pub const MAX_USERNAME_LEN: usize = 32;

/// Longest group name, in bytes.
///
/// Equal to [`MAX_USERNAME_LEN`] because both obey the one identifier
/// grammar, but named separately: they bound different fields and a
/// future grammar split must be expressible.
pub const MAX_GROUPNAME_LEN: usize = 32;

/// Longest account display name, in bytes.
pub const MAX_DISPLAY_NAME_LEN: usize = 64;

/// Longest home or shell path, in bytes.
pub const MAX_PATH_LEN: usize = 128;

/// Most supplementary groups one account may carry.
pub const MAX_SUPPLEMENTARY_GIDS: usize = 16;
