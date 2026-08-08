//! The shared **authentication surface** every place TAIRiX asks a user to
//! prove who they are at the screen (`lib/greeter` —
//! `plans/NEW-DESKTOP-LOGIN.md`).
//!
//! One crate owns what such a surface *is* — one centred column carrying the
//! clock, the date, and the machine name, and under them either the account
//! tiles or the chosen account's disc, name, and masked field — together with
//! the wording, the geometry, and the state machine that turns a keystroke
//! into a verdict. It knows nothing of a compositor, a window manager, a
//! seat, or IPC: an embedder gives it events and a [`Verifier`], takes back a
//! painted [`Surface`] and an [`Outcome`], and owns everything about *where*
//! those pixels and events come from.
//!
//! It lives in `lib/*` because it has two consumers that may not depend on
//! one another: the login greeter service, and the desktop session's screen
//! lock (`userland/gui/session`), which is this surface with the account
//! fixed to the session's own. Two implementations of "prove who you are, at
//! the screen" is the duplication the charter forbids.
//!
//! # What it guarantees
//!
//! * **Only a verified secret concludes it.** [`AuthSurface::on_event`]
//!   reports [`Outcome::verified`] only for a [`Verdict::Verified`] answer.
//!   There is no cancel, no timeout, no error state that falls through: a
//!   refusal, an unreachable authority, an unparseable reply, an empty
//!   account list, and a cooldown running out are all the same answer, and it
//!   is "still asking".
//! * **The secret lives in one place and is erased on every path out.** It
//!   is held only in the masked field's bounded, pre-reserved buffer — which
//!   reserves once so typing can never reallocate and strand a copy in a
//!   freed block, draws beads rather than characters, and redacts itself in
//!   `Debug` — and it is wiped as soon as a verdict comes back, whichever
//!   verdict that is, as well as on every step between accounts.
//! * **The paint and the hit test cannot disagree.** The prompt block, the
//!   field, and the tile rectangles have one definition ([`panel_rect`],
//!   [`AuthSurface::field_rect`], the chooser's own grid), read by both.
//! * **Every length is authored once, in logical pixels.** The whole column
//!   is converted to physical pixels through the one shared
//!   `tairix_geometry::Scale`, so it is the same composition at any DPI.
//! * **It is operable with no pointer at all.** `Tab`, `Shift-Tab`, and the
//!   arrow keys move between tiles, `Return` picks one, and `Escape` steps
//!   back to the chooser — because a machine with no mouse must still log in.
//!
//! What it deliberately does *not* do is rate-limit or count attempts. The
//! authority behind the [`Verifier`] owns that policy and audits every
//! attempt against the account; a second policy here would be a second place
//! to get it wrong. [`AuthSurface::set_cooldown`] shows what the authority
//! reports and refuses to submit while it stands; it reads no clock.
//!
//! Nor does it decode an image. [`Backdrop::Wallpaper`] takes a picture the
//! embedder has already decoded — in its own sandbox — and already fitted to
//! the screen, plus the scrim [`scrim_alpha`] sized for it.
//!
//! # Using it
//!
//! ```
//! use tairix_greeter::{AccountTile, AuthSurface, Backdrop, EventContext, Verdict, Verifier};
//! use tairix_geometry::{Rect, Scale};
//! use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
//! use tairix_theme::Theme;
//!
//! struct Never;
//! impl Verifier for Never {
//!     fn verify(&mut self, _account: &str, _secret: &str) -> Verdict {
//!         Verdict::Refused
//!     }
//! }
//!
//! let theme = Theme::dark();
//! let screen = Rect::new(0, 0, 800, 600);
//! let mut surface = AuthSurface::with_accounts(vec![AccountTile::new("Ann Example", "ann")]);
//! let enter = InputEvent::KeyPressed {
//!     key: Key::Named(NamedKey::Enter),
//!     modifiers: Modifiers::default(),
//! };
//!
//! // The first Return picks the focused tile; the second offers a secret
//! // for the account it named.
//! surface.on_event(
//!     &enter,
//!     &mut EventContext { screen, scale: Scale::ONE, theme: &theme, verifier: &mut Never },
//! );
//! assert_eq!(surface.selected_account(), Some("ann"));
//!
//! let outcome = surface.on_event(
//!     &enter,
//!     &mut EventContext { screen, scale: Scale::ONE, theme: &theme, verifier: &mut Never },
//! );
//! assert!(!outcome.verified());
//! if outcome.redraw() {
//!     let _frame = surface.render(screen, Scale::ONE, &theme, Backdrop::Desktop);
//! }
//! ```
//!
//! [`Surface`]: tairix_raster::Surface

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod chooser;
mod layout;
mod scrim;
mod surface;

pub use chooser::AccountTile;
pub use scrim::scrim_alpha;
pub use surface::{
    panel_rect, AuthSurface, Backdrop, Chrome, EventContext, Outcome, Verdict, Verifier,
    MAX_CHROME, MAX_LOGIN_NAME, MAX_PASSWORD, UNNAMED_ACCOUNT,
};

#[cfg(test)]
mod chooser_tests;
#[cfg(test)]
mod cooldown_tests;
#[cfg(test)]
mod damage_tests;
#[cfg(test)]
mod scrim_tests;
#[cfg(test)]
mod surface_tests;
#[cfg(test)]
mod testkit;
