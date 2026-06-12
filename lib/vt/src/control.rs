//! The raw control bytes and escape introducers of the ANSI / VT / xterm
//! vocabulary, as typed constants.
//!
//! These are the single definition every other module builds on: the emitter
//! writes them and the parser recognises them, so the wire format lives in one
//! place (`AGENTS.md` §2.2). The names follow the ECMA-48 / ANSI X3.64
//! mnemonics.

/// Bell (`BEL`, `^G`).
pub const BEL: u8 = 0x07;
/// Backspace (`BS`, `^H`).
pub const BS: u8 = 0x08;
/// Horizontal tab (`HT`, `^I`).
pub const HT: u8 = 0x09;
/// Line feed (`LF`, `^J`).
pub const LF: u8 = 0x0a;
/// Carriage return (`CR`, `^M`).
pub const CR: u8 = 0x0d;
/// Escape (`ESC`, `^[`) — the introducer for every escape sequence.
pub const ESC: u8 = 0x1b;
/// Delete (`DEL`, `^?`) — the byte most modern terminals send for the
/// Backspace key, and the rub-out control of the read line discipline.
pub const DEL: u8 = 0x7f;

/// The byte after `ESC` that opens a Control Sequence (`ESC [`).
pub const CSI: u8 = b'[';
/// The byte after `ESC` that opens an Operating System Command (`ESC ]`).
pub const OSC: u8 = b']';
/// The byte after `ESC` that opens a Device Control String (`ESC P`).
pub const DCS: u8 = b'P';
/// The byte after `ESC` that opens a Single Shift Three (`ESC O`) — the
/// introducer the function keys `F1`…`F4` and the application-mode editing
/// keys send.
pub const SS3: u8 = b'O';
/// The byte after `ESC` that begins a String Terminator (`ESC \`) — closes an
/// OSC or DCS string.
pub const ST_FINAL: u8 = b'\\';

/// The private-parameter marker that prefixes DEC private mode sequences
/// (`CSI ? … h` / `CSI ? … l`).
pub const PRIVATE: u8 = b'?';
/// The parameter-prefix byte that marks an xterm SGR mouse report
/// (`CSI < Cb ; Cx ; Cy M` / `m`), distinguishing it from an ordinary
/// `CSI … m` Select Graphic Rendition.
pub const MOUSE_SGR: u8 = b'<';
/// The CSI parameter separator (`;`).
pub const SEPARATOR: u8 = b';';

/// `ESC 7` — save cursor position and attributes (DECSC).
pub const SAVE_CURSOR: u8 = b'7';
/// `ESC 8` — restore cursor position and attributes (DECRC).
pub const RESTORE_CURSOR: u8 = b'8';

/// Final byte of Cursor Up (`CSI n A`, CUU).
pub const CUU: u8 = b'A';
/// Final byte of Cursor Down (`CSI n B`, CUD).
pub const CUD: u8 = b'B';
/// Final byte of Cursor Forward (`CSI n C`, CUF).
pub const CUF: u8 = b'C';
/// Final byte of Cursor Back (`CSI n D`, CUB).
pub const CUB: u8 = b'D';
/// Final byte of Cursor Next Line (`CSI n E`, CNL).
pub const CNL: u8 = b'E';
/// Final byte of Cursor Previous Line (`CSI n F`, CPL).
pub const CPL: u8 = b'F';
/// Final byte of Cursor Horizontal Absolute (`CSI n G`, CHA).
pub const CHA: u8 = b'G';
/// Final byte of Cursor Position (`CSI row ; col H`, CUP).
pub const CUP: u8 = b'H';
/// Final byte of Horizontal/Vertical Position (`CSI row ; col f`, HVP) — an
/// alias of [`CUP`].
pub const HVP: u8 = b'f';
/// Final byte of Erase in Display (`CSI n J`, ED).
pub const ED: u8 = b'J';
/// Final byte of Erase in Line (`CSI n K`, EL).
pub const EL: u8 = b'K';
/// Final byte of Scroll Up (`CSI n S`, SU).
pub const SU: u8 = b'S';
/// Final byte of Scroll Down (`CSI n T`, SD).
pub const SD: u8 = b'T';
/// Final byte of Set Top and Bottom Margins (`CSI top ; bottom r`, DECSTBM).
pub const DECSTBM: u8 = b'r';
/// Final byte of Select Graphic Rendition (`CSI … m`, SGR).
pub const SGR: u8 = b'm';
/// Final byte of a DEC private Set Mode (`CSI ? n h`, DECSET).
pub const SET_MODE: u8 = b'h';
/// Final byte of a DEC private Reset Mode (`CSI ? n l`, DECRST).
pub const RESET_MODE: u8 = b'l';

/// Final byte of an extended-key / paste sequence (`CSI <n> ~`, e.g.
/// `CSI 3 ~` = Delete, `CSI 200 ~` = bracketed-paste start).
pub const TILDE: u8 = b'~';
/// Final byte of an xterm mouse *press* report in SGR encoding
/// (`CSI < Cb ; Cx ; Cy M`).
pub const MOUSE_PRESS: u8 = b'M';
/// Final byte of an xterm mouse *release* report in SGR encoding
/// (`CSI < Cb ; Cx ; Cy m`). Distinguished from [`SGR`] by the [`MOUSE_SGR`]
/// prefix.
pub const MOUSE_RELEASE: u8 = b'm';

/// DEC private mode number for "show / hide the cursor" (DECTCEM).
pub const MODE_CURSOR_VISIBLE: u16 = 25;
/// DEC private mode number for xterm "normal" (button press/release) mouse
/// tracking (`CSI ? 1000 h` / `l`).
pub const MODE_MOUSE_BUTTON: u16 = 1000;
/// DEC private mode number for xterm "button-event" (press/release/drag)
/// mouse tracking (`CSI ? 1002 h` / `l`).
pub const MODE_MOUSE_DRAG: u16 = 1002;
/// DEC private mode number for xterm "any-event" (all motion) mouse tracking
/// (`CSI ? 1003 h` / `l`).
pub const MODE_MOUSE_ANY: u16 = 1003;
/// DEC private mode number for the xterm SGR mouse encoding (`CSI ? 1006 h` /
/// `l`), which lifts the 223-column limit of the legacy report.
pub const MODE_MOUSE_SGR: u16 = 1006;
/// DEC private mode number for bracketed paste (`CSI ? 2004 h` / `l`).
pub const MODE_BRACKETED_PASTE: u16 = 2004;
/// DEC private mode number for the xterm alternate screen buffer with
/// save/restore (`CSI ? 1049 h` / `l`).
pub const MODE_ALT_SCREEN: u16 = 1049;

/// Extended-key parameter for bracketed-paste start (`CSI 200 ~`).
pub const PASTE_START: u16 = 200;
/// Extended-key parameter for bracketed-paste end (`CSI 201 ~`).
pub const PASTE_END: u16 = 201;

/// Whether `byte` is a read-line-discipline **erase** (rub-out) control: the
/// Backspace ([`BS`], `^H`) or Delete ([`DEL`], `^?`) a terminal sends to rub
/// out the last character of the line being edited.
///
/// Both are accepted because terminals disagree: a serial terminal commonly
/// sends `BS` for its Backspace key while xterm-class terminals (and the
/// RustOS keymap, which maps `Backspace` to `DEL`) send `DEL`. This is the
/// single definition of "which byte erases" (`AGENTS.md` §2.2), shared by the
/// kernel console echo and a reader's line buffer so the two never disagree.
#[must_use]
pub const fn is_line_erase(byte: u8) -> bool {
    byte == BS || byte == DEL
}

/// The bytes that visually rub out one already-echoed character on a terminal:
/// backspace over the glyph, overwrite it with a space, then backspace again so
/// the cursor rests where the glyph was. The read line discipline writes this
/// when it erases a character so the screen matches the edited line
/// (`AGENTS.md` §20).
pub const ERASE_ECHO: [u8; 3] = [BS, b' ', BS];
