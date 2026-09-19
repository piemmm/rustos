//! A bounded resource pool.
//!
//! Health, and whatever resource an archetype spends, are the same shape: a
//! current value that never goes below zero and never above a maximum. Both
//! are this type, so there is one place the arithmetic can be wrong and one
//! place it is tested.
//!
//! Every operation is total. There is no path that produces a negative value
//! — the type is unsigned and every subtraction saturates — and none that
//! exceeds the maximum, which is what the invariant proptest asserts over
//! arbitrary operation sequences.

/// A current value inside `0..=max`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Pool {
    current: u32,
    max: u32,
}

impl Pool {
    /// A pool at its maximum.
    #[must_use]
    pub const fn full(max: u32) -> Self {
        Self { current: max, max }
    }

    /// A pool at `current`, clamped to `max`.
    #[must_use]
    pub const fn new(current: u32, max: u32) -> Self {
        Self {
            current: if current > max { max } else { current },
            max,
        }
    }

    /// What is in it.
    #[must_use]
    pub const fn current(&self) -> u32 {
        self.current
    }

    /// What it holds when full.
    #[must_use]
    pub const fn max(&self) -> u32 {
        self.max
    }

    /// Whether it is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.current == 0
    }

    /// Take `amount` only if all of it is there, answering whether it was.
    ///
    /// The all-or-nothing form, for a cost that must either be paid or not
    /// attempted: a half-paid cast is not a thing the rules have.
    pub const fn spend(&mut self, amount: u32) -> bool {
        if self.current < amount {
            return false;
        }
        self.current -= amount;
        true
    }

    /// Take up to `amount`, returning how much was actually taken.
    pub const fn drain(&mut self, amount: u32) -> u32 {
        let taken = if amount > self.current {
            self.current
        } else {
            amount
        };
        self.current -= taken;
        taken
    }

    /// Put up to `amount` back, returning how much fitted.
    pub const fn restore(&mut self, amount: u32) -> u32 {
        let room = self.max - self.current;
        let given = if amount > room { room } else { amount };
        self.current += given;
        given
    }

    /// Change the maximum, keeping the current value inside it.
    ///
    /// A pool that shrinks below what it holds spills the difference rather
    /// than carrying a value its own bound forbids.
    pub const fn set_max(&mut self, max: u32) {
        self.max = max;
        if self.current > max {
            self.current = max;
        }
    }

    /// Apply a signed change, saturating at both ends.
    ///
    /// The form the periodic statuses use, where one number can be a
    /// regeneration or a bleed depending on its sign.
    pub fn apply_delta(&mut self, delta: i64) -> u32 {
        if delta >= 0 {
            let amount = u32::try_from(delta).unwrap_or(u32::MAX);
            self.restore(amount)
        } else {
            self.drain(u32::try_from(delta.unsigned_abs()).unwrap_or(u32::MAX))
        }
    }
}

#[cfg(test)]
mod tests;
