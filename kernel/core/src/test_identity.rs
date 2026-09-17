//! Shared host-test identity material.
//!
//! An [`AccountState::Active`](tairix_users::AccountState) record must be
//! login-shaped — home, shell, and a real stored password — so a fixture
//! modelling a human account cannot skip the password. Deriving one costs a
//! PBKDF2 run at the minimum accepted cost, and every fixture that needs one
//! needs the *same* one, so it is derived once for the process.

use tairix_sync::Once;
use tairix_users::{PasswordRecord, StoredPassword, MIN_ITERATIONS};

/// The stored password every host-test human account carries.
///
/// Derived on first use and cloned thereafter: the suites that build these
/// records assert on identity and group resolution, never on the hash, so
/// paying the derivation per record buys nothing.
pub(crate) fn shared_password() -> StoredPassword {
    static PASSWORD: Once<StoredPassword> = Once::new();
    PASSWORD
        .call_once_infallible(|| {
            StoredPassword::Password(
                PasswordRecord::new(b"correct horse", [0x42; 16], MIN_ITERATIONS)
                    .expect("the minimum accepted cost is a valid record"),
            )
        })
        .expect("a fresh cell")
        .clone()
}
