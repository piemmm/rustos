//! What the player pressed, and what the client does about it.
//!
//! Input is *drained* and recorded; nothing here paints, opens a store,
//! or sends a request. A handler updates the held state and returns at
//! most one command, and the frame that follows is produced once from
//! whatever the whole drain left behind.
//!
//! Held keys are state rather than events because movement is continuous:
//! a player holding two arrows is walking diagonally on every tick until
//! one comes up, and that is one direction recomputed rather than a
//! stream of steps.

use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode};
use tairix_abi::window_ipc::{PointerAction, WindowSizeState};
use tairix_wintersun_net::value::Direction;

/// A direction component at full speed.
///
/// The simulation divides a held direction by 32 768 to get sub-units per
/// tick, so this is one whole step; the wire type is signed 16-bit, which
/// is why it is one short of the scale rather than equal to it.
const UNIT: i64 = 32_767;

/// The fixed-point scale the length is computed at.
const LENGTH_SCALE: u32 = 10;

/// Something the client itself does, as opposed to something the
/// simulation does.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Show more ground, or less.
    Zoom(Zoom),
    /// Ask the compositor for a size state.
    Resize(WindowSizeState),
    /// Leave.
    Quit,
}

/// Which way a zoom goes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Zoom {
    /// Closer.
    In,
    /// Further out.
    Out,
}

/// Everything the player is currently holding down.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Controls {
    held: u8,
    pointer: Option<(u32, u32)>,
}

impl Controls {
    /// Nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: 0,
            pointer: None,
        }
    }

    /// Record a key, answering the client command it asked for if any.
    ///
    /// A movement key is state and produces no command; a command key
    /// produces one on the press and nothing on the release, so holding
    /// it does not repeat.
    pub fn apply_key(&mut self, input: &KeyInput) -> Option<Command> {
        let (down, key) = match *input {
            KeyInput::Pressed { key, .. } => (true, key),
            KeyInput::Released { key, .. } => (false, key),
            // A modifier alone moves nothing and commands nothing, but the
            // seat still reports it so a consumer knows what is held.
            KeyInput::ModifiersChanged { .. } => return None,
        };
        if let Some(axis) = movement(key) {
            self.hold(axis, down);
            return None;
        }
        down.then(|| command(key)).flatten()
    }

    /// Record a pointer event, answering the client command it asked for
    /// if any.
    pub fn apply_pointer(&mut self, x: u32, y: u32, action: PointerAction) -> Option<Command> {
        self.pointer = Some((x, y));
        let _ = action;
        None
    }

    /// Where the pointer last was, in window pixels.
    #[must_use]
    pub const fn pointer(&self) -> Option<(u32, u32)> {
        self.pointer
    }

    /// Let go of everything.
    ///
    /// Called when the window loses focus or the seat goes away: the
    /// release of a key held at that moment is delivered to whoever has
    /// the seat next, so a client that kept its held state would walk
    /// into a wall for as long as it was in the background.
    pub fn release_all(&mut self) {
        self.held = 0;
    }

    /// The direction the held keys add up to, at full speed.
    ///
    /// Opposing keys cancel, and a diagonal is normalised rather than
    /// being the faster hypotenuse a naive sum would give.
    #[must_use]
    pub fn direction(&self) -> Direction {
        let x = i64::from(self.holding(Axis::East)) - i64::from(self.holding(Axis::West));
        let y = i64::from(self.holding(Axis::South)) - i64::from(self.holding(Axis::North));
        let (dx, dy) = normalise(x, y);
        Direction::new(dx, dy).unwrap_or_else(|_| Direction::still())
    }

    fn hold(&mut self, axis: Axis, down: bool) {
        if down {
            self.held |= axis.bit();
        } else {
            self.held &= !axis.bit();
        }
    }

    fn holding(&self, axis: Axis) -> bool {
        self.held & axis.bit() != 0
    }
}

/// One of the four movement directions.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Axis {
    North,
    South,
    East,
    West,
}

impl Axis {
    /// Its place in the held set.
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// The movement key `key` is, if it is one.
///
/// Both the arrow cluster and the left-hand cluster, because a player
/// with one hand on a pointer wants the other on `WASD` and a player
/// without wants the arrows.
fn movement(key: KeyValue) -> Option<Axis> {
    match key {
        KeyValue::Named(NamedKeyCode::Up) => Some(Axis::North),
        KeyValue::Named(NamedKeyCode::Down) => Some(Axis::South),
        KeyValue::Named(NamedKeyCode::Left) => Some(Axis::West),
        KeyValue::Named(NamedKeyCode::Right) => Some(Axis::East),
        KeyValue::Char(c) => match c.to_ascii_lowercase() {
            'w' => Some(Axis::North),
            's' => Some(Axis::South),
            'a' => Some(Axis::West),
            'd' => Some(Axis::East),
            _ => None,
        },
        KeyValue::Named(_) => None,
    }
}

/// The client command `key` asks for, if it is one.
fn command(key: KeyValue) -> Option<Command> {
    match key {
        KeyValue::Named(NamedKeyCode::F11) => Some(Command::Resize(WindowSizeState::Fullscreen)),
        KeyValue::Named(NamedKeyCode::Escape) => Some(Command::Resize(WindowSizeState::Restored)),
        KeyValue::Char('+' | '=') => Some(Command::Zoom(Zoom::In)),
        KeyValue::Char('-' | '_') => Some(Command::Zoom(Zoom::Out)),
        KeyValue::Char('q' | 'Q') => Some(Command::Quit),
        _ => None,
    }
}

/// `(x, y)` in `-1..=1` scaled to a full-speed direction.
///
/// The length is rounded *up* before dividing, so a diagonal lands just
/// inside the magnitude the wire type admits rather than a unit past it —
/// the difference is six hundredths of a percent of speed and the
/// alternative is a direction the protocol refuses.
fn normalise(x: i64, y: i64) -> (i16, i16) {
    let length_sq = x * x + y * y;
    if length_sq == 0 {
        return (0, 0);
    }
    let scaled = u64::try_from(length_sq).unwrap_or(1) << (2 * LENGTH_SCALE);
    let root = scaled.isqrt();
    let length = i64::try_from(if root * root == scaled {
        root
    } else {
        root + 1
    })
    .unwrap_or(1);
    let axis =
        |v: i64| i16::try_from(v * UNIT * i64::from(1u32 << LENGTH_SCALE) / length).unwrap_or(0);
    (axis(x), axis(y))
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
