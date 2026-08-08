//! The per-account attempt budget behind a `session-v1` refusal's
//! `retry_after`.
//!
//! Guessing at the graphical login screen costs nothing but keystrokes, so
//! the authority meters it. The budget is deliberately **not** in the
//! greeter: a client-side limit would be duplicated in every surface and
//! trivially bypassed by a caller that simply does not implement it.
//!
//! Three properties make it safe to drive from an untrusted caller's
//! request:
//!
//! * **Per login name.** A wrong password for one account never delays
//!   another, so an attacker cannot lock the machine's users out by
//!   guessing at one of them.
//! * **Monotonic.** Every instant is a caller-supplied [`Duration64`] read
//!   from the monotonic clock, so a cooldown cannot be shortened by moving
//!   the wall clock. The engine reads no clock of its own.
//! * **Bounded.** The tracking table is a fixed [`TRACKED_ACCOUNTS`]-entry
//!   array over attacker-supplied names — a validation bound, not a
//!   capacity that grows on demand.

use tairix_abi::session_ipc::SESSION_LOGIN_NAME_MAX;
use tairix_abi::time::{Duration64, NANOS_PER_SEC};

/// Refusals an account may collect before any cooldown applies.
///
/// Three, matching the text prompt's per-round budget: a mistyped password
/// (or two) is ordinary human behaviour and must not put a delay in front
/// of the person at the machine.
pub const FREE_ATTEMPTS: u32 = 3;

/// Cooldown, in seconds, applied to the first refusal past
/// [`FREE_ATTEMPTS`]. It doubles with each further refusal.
///
/// Five seconds is imperceptible to someone who mistyped once more, and
/// already cuts a scripted guesser's rate by orders of magnitude.
pub const COOLDOWN_BASE_SECS: u64 = 5;

/// Longest cooldown the doubling reaches, in seconds (five minutes).
///
/// Capped rather than unbounded so an account cannot be delayed out of
/// use: whoever is at the machine must always reach a retry within a
/// knowable time.
pub const COOLDOWN_MAX_SECS: u64 = 300;

/// Accounts the budget tracks at once.
///
/// A validation bound over names an untrusted caller chooses, not a
/// capacity: a table that grew on demand would let a caller cycling
/// invented names allocate without limit. Sixteen covers the accounts a
/// seat's users actually cycle between, and the full-table rule below
/// keeps a saturated table both attack-proof and self-clearing.
pub const TRACKED_ACCOUNTS: usize = 16;

/// Doublings applied before [`COOLDOWN_MAX_SECS`] is reached, so the shift
/// can never overflow.
const MAX_DOUBLINGS: u32 = 6;

/// One tracked account's refusal history.
#[derive(Copy, Clone)]
struct Entry {
    /// The account key, stored inline so the table has no heap footprint a
    /// caller's choice of name can grow.
    key: [u8; SESSION_LOGIN_NAME_MAX],
    /// Bytes of [`Entry::key`] in use; zero marks a free slot.
    len: usize,
    /// Refusals recorded against this account.
    failures: u32,
    /// Monotonic instant at which the next attempt is allowed.
    ready_at: Duration64,
}

impl Entry {
    const FREE: Self = Self {
        key: [0; SESSION_LOGIN_NAME_MAX],
        len: 0,
        failures: 0,
        ready_at: Duration64::ZERO,
    };

    fn is_free(&self) -> bool {
        self.len == 0
    }

    fn matches(&self, login_name: &str) -> bool {
        !self.is_free() && self.key[..self.len] == *key_of(login_name)
    }

    fn remaining(&self, now: Duration64) -> Duration64 {
        Duration64::from_nanos(
            self.ready_at
                .saturating_total_nanos()
                .saturating_sub(now.saturating_total_nanos()),
        )
    }
}

/// The authority's per-account guess meter.
///
/// [`AttemptBudget::retry_after`] answers "may this account be offered a
/// secret now?", [`AttemptBudget::note_failure`] records a refusal and
/// returns the resulting cooldown, and [`AttemptBudget::note_success`]
/// clears the account. The value a refusal reports is what the login
/// screen displays as its cooldown; nothing else reads it.
pub struct AttemptBudget {
    entries: [Entry; TRACKED_ACCOUNTS],
}

impl Default for AttemptBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl AttemptBudget {
    /// An empty budget: every account may be offered a secret at once.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [Entry::FREE; TRACKED_ACCOUNTS],
        }
    }

    /// How long `login_name` must wait before another secret may be offered
    /// for it, [`Duration64::ZERO`] when one may be offered now.
    ///
    /// An untracked name waits only when the table is full **and** every
    /// entry in it is still cooling down; it then inherits the table's
    /// *minimum* remaining cooldown. That is what stops a caller cycling
    /// invented names from buying itself unmetered guesses, and it can
    /// never become a standing lockout: the wait is by construction the
    /// time until the soonest entry frees its slot.
    #[must_use]
    pub fn retry_after(&self, login_name: &str, now: Duration64) -> Duration64 {
        if let Some(entry) = self.entries.iter().find(|entry| entry.matches(login_name)) {
            return entry.remaining(now);
        }
        if self.claimable(now).is_some() {
            Duration64::ZERO
        } else {
            self.min_remaining(now)
        }
    }

    /// Record one refused attempt for `login_name`, returning how long it
    /// must now wait.
    ///
    /// An attempt made while the account is already cooling down does not
    /// extend the cooldown — the caller is told to wait, and waiting is all
    /// it can do. Only an adjudicated attempt adds to the count.
    pub fn note_failure(&mut self, login_name: &str, now: Duration64) -> Duration64 {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.matches(login_name))
        {
            if entry.remaining(now) == Duration64::ZERO {
                entry.failures = entry.failures.saturating_add(1);
                entry.ready_at = after(now, cooldown_secs(entry.failures));
            }
            return entry.remaining(now);
        }
        let Some(slot) = self.claimable(now) else {
            // Every tracked account is still cooling down, so no slot can be
            // taken without discarding a live cooldown. The newcomer waits
            // out the shortest of them instead: bounded, and the table keeps
            // the cooldowns it earned.
            return self.min_remaining(now);
        };
        let key = key_of(login_name);
        let entry = &mut self.entries[slot];
        *entry = Entry::FREE;
        entry.key[..key.len()].copy_from_slice(key);
        entry.len = key.len();
        entry.failures = 1;
        entry.ready_at = after(now, cooldown_secs(1));
        entry.remaining(now)
    }

    /// Forget `login_name`'s refusals: it authenticated.
    pub fn note_success(&mut self, login_name: &str) {
        for entry in &mut self.entries {
            if entry.matches(login_name) {
                *entry = Entry::FREE;
            }
        }
    }

    /// The slot a newcomer would take: a free one, else the entry that
    /// stopped cooling down longest ago. `None` when every entry is still
    /// cooling down.
    ///
    /// Oldest-expiry-first matters: a heavily-guessed account has the
    /// longest cooldown and therefore the latest deadline, so it is the
    /// **last** entry a caller churning invented names can displace. It
    /// must exhaust its own stale entries before it can reach an escalated
    /// account's history.
    fn claimable(&self, now: Duration64) -> Option<usize> {
        if let Some(free) = self.entries.iter().position(Entry::is_free) {
            return Some(free);
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.remaining(now) == Duration64::ZERO)
            .min_by_key(|(index, entry)| (entry.ready_at, *index))
            .map(|(index, _)| index)
    }

    /// The shortest cooldown still running anywhere in the table.
    fn min_remaining(&self, now: Duration64) -> Duration64 {
        self.entries
            .iter()
            .filter(|entry| !entry.is_free())
            .map(|entry| entry.remaining(now))
            .min()
            .unwrap_or(Duration64::ZERO)
    }
}

/// The bytes an account is tracked under: the name, clipped to the wire
/// bound so every key fits an entry.
///
/// The protocol refuses a longer name at decode, so clipping is reachable
/// only from a direct caller. Two over-long names sharing a clipped prefix
/// then share one meter, which is stricter than tracking them apart and
/// can never let either escape the budget.
fn key_of(login_name: &str) -> &[u8] {
    let bytes = login_name.as_bytes();
    &bytes[..bytes.len().min(SESSION_LOGIN_NAME_MAX)]
}

/// The cooldown `failures` refusals earn, in seconds: nothing for the free
/// attempts, then [`COOLDOWN_BASE_SECS`] doubling to [`COOLDOWN_MAX_SECS`].
fn cooldown_secs(failures: u32) -> u64 {
    let over = failures.saturating_sub(FREE_ATTEMPTS);
    if over == 0 {
        return 0;
    }
    let doublings = (over - 1).min(MAX_DOUBLINGS);
    (COOLDOWN_BASE_SECS << doublings).min(COOLDOWN_MAX_SECS)
}

/// `now` advanced by `secs`, saturating rather than wrapping.
fn after(now: Duration64, secs: u64) -> Duration64 {
    Duration64::from_nanos(
        now.saturating_total_nanos()
            .saturating_add(secs.saturating_mul(u64::from(NANOS_PER_SEC))),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        cooldown_secs, AttemptBudget, COOLDOWN_BASE_SECS, COOLDOWN_MAX_SECS, FREE_ATTEMPTS,
        SESSION_LOGIN_NAME_MAX, TRACKED_ACCOUNTS,
    };
    use alloc::format;
    use tairix_abi::time::Duration64;

    fn at(secs: i64) -> Duration64 {
        Duration64::from_secs(secs)
    }

    fn secs(value: u64) -> Duration64 {
        Duration64::from_secs(i64::try_from(value).expect("a cooldown fits a signed second count"))
    }

    /// Refuse `count` attempts for `login_name`, all at `now`.
    fn refuse(budget: &mut AttemptBudget, login_name: &str, count: u32, now: Duration64) {
        for _ in 0..count {
            let _ = budget.note_failure(login_name, now);
        }
    }

    #[test]
    fn the_free_attempts_cost_nothing() {
        let mut budget = AttemptBudget::new();
        for _ in 0..FREE_ATTEMPTS {
            assert_eq!(budget.note_failure("ada", at(0)), Duration64::ZERO);
            assert_eq!(budget.retry_after("ada", at(0)), Duration64::ZERO);
        }
    }

    #[test]
    fn the_cooldown_doubles_from_the_base_and_stops_at_the_cap() {
        let mut budget = AttemptBudget::new();
        refuse(&mut budget, "ada", FREE_ATTEMPTS, at(0));
        let mut now = 0i64;
        let mut expected = COOLDOWN_BASE_SECS;
        loop {
            assert_eq!(
                budget.note_failure("ada", at(now)),
                secs(expected),
                "cooldown at t={now}"
            );
            if expected == COOLDOWN_MAX_SECS {
                break;
            }
            now += i64::try_from(expected).expect("fits");
            expected = (expected * 2).min(COOLDOWN_MAX_SECS);
        }
        // Past the cap the wait stays there rather than growing without end.
        now += i64::try_from(COOLDOWN_MAX_SECS).expect("fits");
        assert_eq!(budget.note_failure("ada", at(now)), secs(COOLDOWN_MAX_SECS));
    }

    #[test]
    fn a_cooldown_expires_as_the_monotonic_clock_advances() {
        let mut budget = AttemptBudget::new();
        refuse(&mut budget, "ada", FREE_ATTEMPTS + 1, at(0));
        let base = i64::try_from(COOLDOWN_BASE_SECS).expect("fits");
        assert_eq!(budget.retry_after("ada", at(0)), secs(COOLDOWN_BASE_SECS));
        assert_eq!(budget.retry_after("ada", at(base - 1)), at(1));
        assert_eq!(budget.retry_after("ada", at(base)), Duration64::ZERO);
        assert_eq!(budget.retry_after("ada", at(base + 60)), Duration64::ZERO);
    }

    #[test]
    fn an_attempt_during_a_cooldown_does_not_extend_it() {
        let mut budget = AttemptBudget::new();
        refuse(&mut budget, "ada", FREE_ATTEMPTS + 1, at(0));
        let base = i64::try_from(COOLDOWN_BASE_SECS).expect("fits");
        for probe in 1..base {
            assert_eq!(budget.note_failure("ada", at(probe)), at(base - probe));
        }
        assert_eq!(budget.retry_after("ada", at(base)), Duration64::ZERO);
    }

    #[test]
    fn one_accounts_cooldown_never_delays_another() {
        let mut budget = AttemptBudget::new();
        refuse(&mut budget, "ada", FREE_ATTEMPTS + 1, at(0));
        assert!(budget.retry_after("ada", at(0)) > Duration64::ZERO);
        assert_eq!(budget.retry_after("grace", at(0)), Duration64::ZERO);
    }

    #[test]
    fn a_success_clears_that_accounts_entry_only() {
        let mut budget = AttemptBudget::new();
        refuse(&mut budget, "ada", FREE_ATTEMPTS + 1, at(0));
        refuse(&mut budget, "grace", FREE_ATTEMPTS + 1, at(0));
        budget.note_success("ada");
        assert_eq!(budget.retry_after("ada", at(0)), Duration64::ZERO);
        assert!(budget.retry_after("grace", at(0)) > Duration64::ZERO);
        // The cleared account starts again from its free attempts.
        assert_eq!(budget.note_failure("ada", at(0)), Duration64::ZERO);
    }

    /// Fill every slot with an account that is still cooling down at `t=0`.
    fn saturate(budget: &mut AttemptBudget) {
        for index in 0..TRACKED_ACCOUNTS {
            refuse(budget, &format!("user{index}"), FREE_ATTEMPTS + 1, at(0));
        }
    }

    #[test]
    fn a_full_table_evicts_an_entry_whose_cooldown_has_expired() {
        let mut budget = AttemptBudget::new();
        saturate(&mut budget);
        // Long after every cooldown lapsed, a newcomer simply takes a slot.
        let later = at(i64::try_from(COOLDOWN_MAX_SECS).expect("fits") * 2);
        assert_eq!(budget.retry_after("newcomer", later), Duration64::ZERO);
        assert_eq!(budget.note_failure("newcomer", later), Duration64::ZERO);
        assert_eq!(budget.retry_after("newcomer", later), Duration64::ZERO);
    }

    #[test]
    fn a_wholly_cooling_table_charges_a_newcomer_the_shortest_remaining_wait() {
        let mut budget = AttemptBudget::new();
        saturate(&mut budget);
        let shortest = budget.retry_after("user0", at(0));
        assert_eq!(shortest, secs(COOLDOWN_BASE_SECS));
        // Cycling a fresh name buys no unmetered guess, and does not evict
        // the account whose cooldown it inherits.
        assert_eq!(budget.retry_after("mallory", at(0)), shortest);
        assert_eq!(budget.note_failure("mallory", at(0)), shortest);
        assert_eq!(budget.retry_after("user0", at(0)), shortest);
        // The wait is bounded: when the shortest lapses, a slot frees.
        let base = i64::try_from(COOLDOWN_BASE_SECS).expect("fits");
        assert_eq!(budget.retry_after("mallory", at(base)), Duration64::ZERO);
    }

    #[test]
    fn the_entry_expired_longest_is_the_one_a_newcomer_displaces() {
        let mut budget = AttemptBudget::new();
        // One account stops cooling at t=5, the rest at t=10.
        refuse(&mut budget, "user0", FREE_ATTEMPTS + 1, at(0));
        for index in 1..TRACKED_ACCOUNTS {
            refuse(
                &mut budget,
                &format!("user{index}"),
                FREE_ATTEMPTS + 1,
                at(5),
            );
        }
        // Every entry has expired, so the newcomer takes the oldest.
        let _ = budget.note_failure("mallory", at(20));
        // The displaced account starts again from its free attempts. Taking
        // a slot for it displaces the next-oldest in turn, so only the
        // youngest entry is certain to have survived both.
        assert_eq!(budget.note_failure("user0", at(20)), Duration64::ZERO);
        // That survivor keeps its escalating count.
        let youngest = format!("user{}", TRACKED_ACCOUNTS - 1);
        assert_eq!(
            budget.note_failure(&youngest, at(20)),
            secs(COOLDOWN_BASE_SECS * 2)
        );
    }

    #[test]
    fn a_name_longer_than_the_wire_bound_is_still_metered() {
        let mut budget = AttemptBudget::new();
        let long = "a".repeat(SESSION_LOGIN_NAME_MAX * 2);
        refuse(&mut budget, &long, FREE_ATTEMPTS + 1, at(0));
        assert!(budget.retry_after(&long, at(0)) > Duration64::ZERO);
        // It occupies one slot, keyed by its clipped prefix, rather than a
        // fresh slot per attempt.
        assert!(budget.retry_after(&long[..SESSION_LOGIN_NAME_MAX], at(0)) > Duration64::ZERO);
    }

    #[test]
    fn the_cooldown_schedule_is_the_documented_one() {
        assert_eq!(cooldown_secs(0), 0);
        assert_eq!(cooldown_secs(FREE_ATTEMPTS), 0);
        assert_eq!(cooldown_secs(FREE_ATTEMPTS + 1), COOLDOWN_BASE_SECS);
        assert_eq!(cooldown_secs(FREE_ATTEMPTS + 2), COOLDOWN_BASE_SECS * 2);
        assert_eq!(cooldown_secs(FREE_ATTEMPTS + 3), COOLDOWN_BASE_SECS * 4);
        assert_eq!(cooldown_secs(u32::MAX), COOLDOWN_MAX_SECS);
    }
}
