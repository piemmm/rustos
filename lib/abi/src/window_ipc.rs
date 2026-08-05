//! The window-channel IPC protocol (`plans/APPWIN.md` AW2): the reserved
//! rendezvous the desktop session binds and the fixed-width, fail-closed
//! requests an application presents its windows through.
//!
//! The transport reuses the display-service zero-copy shape: an app
//! `shm_create`s a region holding its window frames, hands the session the
//! endpoint-directed `shm_grant` handle once (`Create`), and thereafter
//! presents by **frame index** plus a damage rectangle (`Present`) — no
//! pixel bytes ever cross the IPC. The session keys every window to the
//! kernel-attested identity of the task that created it (`call_peer_origin`),
//! never to anything claimed on the wire, so one app can never present or
//! close another's window.
//!
//! Input travels the other way: the session encodes each routed event as a
//! fixed-width [`WindowEvent`] and sends it to the owning app's own event
//! endpoint (named in `Create`), where the app parks until one arrives.
//! Events are advisory data about the app's own windows; they carry no
//! ambient authority and no secret. The one authority-adjacent field — the
//! [`WindowEvent::FilePicked`] delegation handle — is owner-bound
//! kernel-side (it redeems only when presented by the task it was minted
//! to, `fd_redeem`), so the number is useless to any observer or forger;
//! an app still accepts events only from the session identity the create
//! reply named.
//!
//! Requests are the fixed-width [`WindowRequest`]. `Create` answers with
//! the [`WINDOW_CREATE_REPLY_LEN`]-byte window-id reply
//! ([`encode_create_reply`] / [`decode_create_reply`]); `Present` and
//! `Close` answer with the shared status frame
//! ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]). Every decode fails closed: an
//! unknown magic, version, operation, format, an out-of-bounds frame
//! count, an empty damage rectangle, a malformed title, or a dirty
//! reserved field refuses rather than guessing.

use crate::desktop::DesktopInfo;
use crate::driver::display::{DamageRect, DisplayFormat};
use crate::input::KeyInput;
use crate::input::PointerButtonCode;
use crate::le::{put_i32, put_u16, put_u32, put_u64, read_i32, read_u16, read_u32, read_u64};
use crate::{Errno, ProcId};

/// Reserved well-known call-endpoint id of the desktop session's window
/// service (`"WI"` ASCII hex-spelled prefix, mirroring
/// [`crate::seat::SEATMGR_ENDPOINT`]'s convention). Like the
/// notification and Switchboard tray-summary rendezvous it is
/// **seat-scoped** ([`crate::ipc::is_reserved_endpoint`],
/// [`crate::ipc::is_seat_scoped_endpoint`]): the kernel authorises its
/// bind either by `CAP_IPC_BIND_PRIVILEGED`
/// or by the caller's kernel-attested **live seat lease** — the desktop
/// session that owns the seat serves the windows shown on it, and
/// nothing else may. A squatter claiming the rendezvous first would
/// receive every app's shared-surface grants and could feed apps
/// fabricated input events, so an unentitled bind fails closed.
pub const WINDOW_ENDPOINT: u64 = 0x5749_1001;

/// Magic number identifying a window-channel request (`"WIN1"`
/// little-endian).
pub const WINDOW_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"WIN1");

/// Magic number identifying a window-channel event (`"WEV1"`
/// little-endian).
pub const WINDOW_EVENT_MAGIC: u32 = u32::from_le_bytes(*b"WEV1");

/// The `window-v1` protocol version.
pub const WINDOW_VERSION_V1: u16 = 1;

/// Most frames one `Create` may lay out in its shared region. A validation
/// bound, not a capacity: two frames are the double-buffer steady state,
/// and anything beyond four buys no latency while letting a hostile app
/// reserve unbounded pinned memory. Deliberately its own constant — the
/// display protocol's bound merely coincides today and the two may
/// diverge.
pub const WINDOW_MAX_FRAMES: u32 = 4;

/// Maximum request, in bytes, the [`WINDOW_ENDPOINT`] accepts: exactly one
/// fixed-width [`WindowRequest`].
pub const WINDOW_MAX_REQUEST: usize = WindowRequest::WIRE_LEN;

/// Maximum encoded length, in bytes, of a window title.
pub const WINDOW_TITLE_MAX: usize = 64;

/// Maximum encoded length, in bytes, of a taskbar pin/drag bundle path.
///
/// Deliberately its own constant, mirroring `lib/proglib`'s catalog-engine
/// bundle-path bound: this crate cannot import that bound without inverting
/// the dependency layering (`lib/proglib` depends on `lib/abi`, never the
/// reverse), so the two are independently maintained. A path longer than
/// this can never name a real store bundle, so nothing legitimate is ever
/// refused by the bound.
pub const WINDOW_BUNDLE_PATH_MAX: usize = 512;

/// A validated window title: bounded UTF-8 with no control characters.
///
/// The title crosses a trust boundary into the session's taskbar and
/// window chrome, so it is validated at construction and again at decode:
/// at most [`WINDOW_TITLE_MAX`] bytes, well-formed UTF-8, and no control
/// characters (no escape sequences, no line breaks) — a malformed title
/// is refused, never sanitised.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct WindowTitle {
    bytes: [u8; WINDOW_TITLE_MAX],
    len: u8,
}

impl WindowTitle {
    /// Build a title from `text`, validating length and content.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — longer than [`WINDOW_TITLE_MAX`]
    ///   bytes when UTF-8 encoded.
    /// * [`Errno::OutOfRange`] — contains a control character.
    pub fn new(text: &str) -> Result<Self, Errno> {
        let len = u8::try_from(text.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if text.len() > WINDOW_TITLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if text.chars().any(char::is_control) {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; WINDOW_TITLE_MAX];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self { bytes, len })
    }

    /// The title text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer was validated as UTF-8 at construction/decode; an
        // impossible failure yields the empty title, never a panic.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Decode a title from its fixed-width wire image: one length byte's
    /// worth of validated text, with the tail required zero.
    fn from_wire(len: u8, bytes: &[u8; WINDOW_TITLE_MAX]) -> Result<Self, Errno> {
        let len_usize = usize::from(len);
        if len_usize > WINDOW_TITLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[len_usize..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let text = core::str::from_utf8(&bytes[..len_usize]).map_err(|_| Errno::OutOfRange)?;
        if text.chars().any(char::is_control) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { bytes: *bytes, len })
    }
}

impl core::fmt::Debug for WindowTitle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("WindowTitle").field(&self.as_str()).finish()
    }
}

/// A validated taskbar pin/drag bundle-path reference: bounded UTF-8 with
/// no control characters and no `#` (the resource-reference fragment
/// separator, so a pinned path can never be mistaken for one).
///
/// The path crosses a trust boundary into the session's pin store and drag
/// routing, so it is validated at construction and again at decode: at
/// least one and at most [`WINDOW_BUNDLE_PATH_MAX`] bytes, well-formed
/// UTF-8, no control characters, and no `#` — a malformed path is refused,
/// never sanitised.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct BundleRef {
    bytes: [u8; WINDOW_BUNDLE_PATH_MAX],
    len: u16,
}

impl BundleRef {
    /// Build a bundle reference from `text`, validating length and content.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — empty, or longer than
    ///   [`WINDOW_BUNDLE_PATH_MAX`] bytes when UTF-8 encoded.
    /// * [`Errno::OutOfRange`] — contains a control character or `#`.
    pub fn new(text: &str) -> Result<Self, Errno> {
        let len = u16::try_from(text.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if text.is_empty() || text.len() > WINDOW_BUNDLE_PATH_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if text.chars().any(|c| char::is_control(c) || c == '#') {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; WINDOW_BUNDLE_PATH_MAX];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self { bytes, len })
    }

    /// The path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer was validated as UTF-8 at construction/decode; an
        // impossible failure yields the empty path, never a panic.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Decode a bundle reference from its fixed-width wire image: one
    /// length prefix's worth of validated text, with the tail required
    /// zero.
    fn from_wire(len: u16, bytes: &[u8; WINDOW_BUNDLE_PATH_MAX]) -> Result<Self, Errno> {
        let len_usize = usize::from(len);
        if len_usize == 0 || len_usize > WINDOW_BUNDLE_PATH_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[len_usize..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let text = core::str::from_utf8(&bytes[..len_usize]).map_err(|_| Errno::OutOfRange)?;
        if text.chars().any(|c| char::is_control(c) || c == '#') {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { bytes: *bytes, len })
    }
}

impl core::fmt::Debug for BundleRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("BundleRef").field(&self.as_str()).finish()
    }
}

/// One window-channel operation (`plans/APPWIN.md` AW2).
///
/// Every request acts on the caller's **own** windows: the session derives
/// ownership from the kernel-attested identity of the in-flight caller,
/// never from a claimed id, so the window id here is a name, not a
/// credential.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WindowRequest {
    /// Open a window over the caller's granted frame region: map the
    /// region once, validate the frame layout, and list the window under
    /// `title`. The session assigns and replies the window id.
    Create {
        /// The `shm_grant` handle minted to the session's serving task,
        /// naming the region that holds the window's frames back-to-back.
        shm_handle: u64,
        /// The caller's own endpoint the session delivers this window's
        /// [`WindowEvent`]s to. Never a reserved endpoint.
        event_endpoint: u64,
        /// Frames laid out back-to-back in the region
        /// (`1..=WINDOW_MAX_FRAMES`).
        frame_count: u32,
        /// Window width in pixels; never zero.
        width_px: u32,
        /// Window height in pixels; never zero.
        height_px: u32,
        /// Bytes between consecutive scanlines; at least one scanline.
        stride_bytes: u32,
        /// Pixel encoding of the frames.
        format: DisplayFormat,
        /// The window's title, listed on the taskbar.
        title: WindowTitle,
        /// Whether the app wants the window manager to present the window
        /// as resizable — a drawn resize grabber and a live maximize/
        /// restore size toggle. A resizable app re-lays-out to each new
        /// client size the window manager reports
        /// ([`WindowEvent::Resized`]), re-mapping its frame region with
        /// [`Self::Resize`]. A fixed-size app leaves this `false`: the
        /// window manager offers neither affordance and never sends it a
        /// size change.
        resizable: bool,
    },
    /// Show frame `frame_index` of window `window_id`, of which only
    /// `damage` changed since the previously presented frame.
    Present {
        /// The window being presented (from the `Create` reply).
        window_id: u64,
        /// Index of the frame inside the window's region.
        frame_index: u32,
        /// The changed rectangle; never empty.
        damage: DamageRect,
    },
    /// Close window `window_id`, tearing down its region mapping and its
    /// taskbar entry.
    Close {
        /// The window being closed.
        window_id: u64,
    },
    /// Re-map window `window_id`'s frame region at a new geometry, keeping
    /// the same window id, owner, event endpoint, and taskbar entry.
    ///
    /// A resizable app issues this after the window manager tells it a new
    /// client size (`WindowEvent::Resized`): it allocates a fresh frame
    /// region of the new geometry, grants it to the session, and re-maps
    /// the *existing* window onto it, so a resize/maximize keeps the window
    /// identity rather than opening a new window. The session drops the old
    /// mapping and adopts the new one; the frame layout is validated
    /// exactly as [`Self::Create`].
    Resize {
        /// The window being resized (from the `Create` reply).
        window_id: u64,
        /// The `shm_grant` handle for the new frame region.
        shm_handle: u64,
        /// Frames laid out back-to-back in the new region
        /// (`1..=WINDOW_MAX_FRAMES`).
        frame_count: u32,
        /// New window width in pixels; never zero.
        width_px: u32,
        /// New window height in pixels; never zero.
        height_px: u32,
        /// Bytes between consecutive scanlines; at least one scanline.
        stride_bytes: u32,
        /// Pixel encoding of the new frames.
        format: DisplayFormat,
    },
    /// Ask the session to run its **trusted file picker** for window
    /// `window_id` (`plans/CAPABILITY_USE.md` CU6). The reply is only the
    /// acceptance: the pick is asynchronous — the user browses in the
    /// session's own UI under the session's own authority — and concludes
    /// with a [`WindowEvent::FilePicked`] (carrying a one-shot `fd_redeem`
    /// handle for the chosen file) or a [`WindowEvent::PickCancelled`]
    /// delivered to the window's event endpoint. One pick may be pending
    /// per window; a second request while one is pending is refused
    /// (`AlreadyExists`).
    PickFile {
        /// The requesting app's own window the pick concludes to.
        window_id: u64,
    },
    /// A user gesture in this window asked to pin `path` to the taskbar
    /// (`plans/NEW-TASKBAR.md` T7). The session validates that the bundle
    /// exists and is launchable and adds the pin, replying the outcome as
    /// the shared status frame: `Errno::AlreadyExists` when the bundle is
    /// already pinned, `Errno::NoSpace` when the pin store is full, or
    /// `Errno::PermissionDenied` when the host refuses pinning outright.
    PinBundle {
        /// The requesting app's own window the gesture originated in.
        window: u64,
        /// The bundle path offered for pinning.
        path: BundleRef,
    },
    /// An app-reference drag naming `path` started in this window; a
    /// primary release may drop it onto the taskbar's pin strip. The
    /// session tracks the offer against the window until it is withdrawn
    /// ([`Self::DragWithdraw`]) or the drop concludes elsewhere.
    DragOffer {
        /// The requesting app's own window the drag originated in.
        window: u64,
        /// The bundle path being dragged.
        path: BundleRef,
    },
    /// The drag started by a preceding [`Self::DragOffer`] on this window
    /// was cancelled by the app itself (e.g. the user pressed Escape
    /// before releasing); disarm the offer without a drop.
    DragWithdraw {
        /// The window whose offer is withdrawn.
        window: u64,
    },
    /// Describe the desktop the caller's windows are displayed on: the
    /// screen extent, the UI scale, and the active appearance
    /// ([`DesktopInfo`]). The reply is the
    /// [`WINDOW_DESKTOP_REPLY_LEN`]-byte desktop frame
    /// ([`encode_desktop_reply`] / [`decode_desktop_reply`]).
    ///
    /// Read-only, and the one request that names no window: an app asks
    /// *before* it opens anything, so its first frame is already the right
    /// size, at the right density, in the right colours rather than a
    /// guess it must correct. Thereafter the session pushes a
    /// [`WindowEvent::DesktopChanged`] to each of the app's windows when
    /// any of it changes.
    ///
    /// It carries no capability: the reply describes the seat's own screen
    /// and theme — no other principal's data, and no authority to act — so
    /// gating it would only force every application to guess at facts the
    /// user can see by looking at their monitor.
    QueryDesktop,
}

/// Wire operation discriminant of [`WindowRequest::Create`].
const OP_CREATE: u16 = 1;
/// Wire operation discriminant of [`WindowRequest::Present`].
const OP_PRESENT: u16 = 2;
/// Wire operation discriminant of [`WindowRequest::Close`].
const OP_CLOSE: u16 = 3;
/// Wire operation discriminant of [`WindowRequest::PickFile`].
const OP_PICK_FILE: u16 = 4;
/// Wire operation discriminant of [`WindowRequest::Resize`].
const OP_RESIZE: u16 = 5;
/// Wire operation discriminant of [`WindowRequest::PinBundle`].
const OP_PIN_BUNDLE: u16 = 6;
/// Wire operation discriminant of [`WindowRequest::DragOffer`].
const OP_DRAG_OFFER: u16 = 7;
/// Wire operation discriminant of [`WindowRequest::DragWithdraw`].
const OP_DRAG_WITHDRAW: u16 = 8;
/// Wire operation discriminant of [`WindowRequest::QueryDesktop`].
const OP_QUERY_DESKTOP: u16 = 9;

/// Byte offset, within the fixed frame, of a bundle-path payload's
/// length-prefixed text — shared by [`WindowRequest::PinBundle`] and
/// [`WindowRequest::DragOffer`], which carry an identical window id +
/// path shape.
const BUNDLE_PATH_OFFSET: usize = 18;

impl WindowRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), and a
    /// 522-byte operation block whose unused tail must be zero (a pin/drag
    /// bundle path is now the widest: an 8-byte window id, a 2-byte
    /// length, and up to [`WINDOW_BUNDLE_PATH_MAX`] path bytes).
    pub const WIRE_LEN: usize = 4 + 2 + 2 + 8 + 2 + WINDOW_BUNDLE_PATH_MAX;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, WINDOW_REQUEST_MAGIC);
        put_u16(&mut out, 4, WINDOW_VERSION_V1);
        match *self {
            Self::Create {
                shm_handle,
                event_endpoint,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
                title,
                resizable,
            } => {
                put_u16(&mut out, 6, OP_CREATE);
                put_u64(&mut out, 8, shm_handle);
                put_u64(&mut out, 16, event_endpoint);
                put_u32(&mut out, 24, frame_count);
                put_u32(&mut out, 28, width_px);
                put_u32(&mut out, 32, height_px);
                put_u32(&mut out, 36, stride_bytes);
                out[40] = format.as_u8();
                out[41] = title.len;
                out[42..42 + WINDOW_TITLE_MAX].copy_from_slice(&title.bytes);
                out[42 + WINDOW_TITLE_MAX] = u8::from(resizable);
            }
            Self::Present {
                window_id,
                frame_index,
                damage,
            } => {
                put_u16(&mut out, 6, OP_PRESENT);
                put_u64(&mut out, 8, window_id);
                put_u32(&mut out, 16, frame_index);
                put_u32(&mut out, 20, damage.x);
                put_u32(&mut out, 24, damage.y);
                put_u32(&mut out, 28, damage.width_px);
                put_u32(&mut out, 32, damage.height_px);
            }
            Self::Close { window_id } => {
                put_u16(&mut out, 6, OP_CLOSE);
                put_u64(&mut out, 8, window_id);
            }
            Self::PickFile { window_id } => {
                put_u16(&mut out, 6, OP_PICK_FILE);
                put_u64(&mut out, 8, window_id);
            }
            Self::Resize {
                window_id,
                shm_handle,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            } => {
                put_u16(&mut out, 6, OP_RESIZE);
                put_u64(&mut out, 8, window_id);
                put_u64(&mut out, 16, shm_handle);
                put_u32(&mut out, 24, frame_count);
                put_u32(&mut out, 28, width_px);
                put_u32(&mut out, 32, height_px);
                put_u32(&mut out, 36, stride_bytes);
                out[40] = format.as_u8();
            }
            Self::PinBundle { window, path } => {
                put_u16(&mut out, 6, OP_PIN_BUNDLE);
                encode_bundle_path(&mut out, window, &path);
            }
            Self::DragOffer { window, path } => {
                put_u16(&mut out, 6, OP_DRAG_OFFER);
                encode_bundle_path(&mut out, window, &path);
            }
            Self::DragWithdraw { window } => {
                put_u16(&mut out, 6, OP_DRAG_WITHDRAW);
                put_u64(&mut out, 8, window);
            }
            Self::QueryDesktop => {
                put_u16(&mut out, 6, OP_QUERY_DESKTOP);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// Semantic bounds a decoder can already see are enforced here — the
    /// frame count within `1..=WINDOW_MAX_FRAMES`, a plausible geometry
    /// (no zero extent, a stride that holds one scanline), a valid title,
    /// a non-reserved event endpoint, a non-zero window id, a non-empty
    /// damage rectangle — so no accepted request ever carries a value the
    /// session would have to re-reject structurally. Bounds only the
    /// session knows (which windows exist, who owns them, the configured
    /// frame count) stay server-side.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, a dirty reserved tail, a dirty
    ///   title tail, or a dirty bundle-path tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `window-v1`.
    /// * [`Errno::OutOfRange`] — an operation or pixel format outside the
    ///   closed set, a malformed title, a zero window id, a reserved event
    ///   endpoint, or a bundle path that is not UTF-8 or holds a control
    ///   character or `#`.
    /// * [`Errno::LengthOutOfRange`] — a frame count outside
    ///   `1..=WINDOW_MAX_FRAMES`, a zero-extent geometry, a stride too
    ///   small for one scanline, an over-long title length, an empty
    ///   damage rectangle, or a bundle-path length outside
    ///   `1..=WINDOW_BUNDLE_PATH_MAX`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != WINDOW_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != WINDOW_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        match op {
            OP_CREATE => read_create(bytes),
            OP_PRESENT => {
                reserved_zero(bytes, 36)?;
                let window_id = nonzero_window_id(read_u64(bytes, 8))?;
                let frame_index = read_u32(bytes, 16);
                let damage = DamageRect {
                    x: read_u32(bytes, 20),
                    y: read_u32(bytes, 24),
                    width_px: read_u32(bytes, 28),
                    height_px: read_u32(bytes, 32),
                };
                if damage.width_px == 0 || damage.height_px == 0 {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::Present {
                    window_id,
                    frame_index,
                    damage,
                })
            }
            OP_CLOSE => {
                reserved_zero(bytes, 16)?;
                let window_id = nonzero_window_id(read_u64(bytes, 8))?;
                Ok(Self::Close { window_id })
            }
            OP_PICK_FILE => {
                reserved_zero(bytes, 16)?;
                let window_id = nonzero_window_id(read_u64(bytes, 8))?;
                Ok(Self::PickFile { window_id })
            }
            OP_RESIZE => {
                reserved_zero(bytes, 41)?;
                let window_id = nonzero_window_id(read_u64(bytes, 8))?;
                let shm_handle = read_u64(bytes, 16);
                let layout = read_frame_layout(bytes)?;
                Ok(Self::Resize {
                    window_id,
                    shm_handle,
                    frame_count: layout.frame_count,
                    width_px: layout.width_px,
                    height_px: layout.height_px,
                    stride_bytes: layout.stride_bytes,
                    format: layout.format,
                })
            }
            OP_PIN_BUNDLE => {
                let (window, path) = read_bundle_path(bytes)?;
                Ok(Self::PinBundle { window, path })
            }
            OP_DRAG_OFFER => {
                let (window, path) = read_bundle_path(bytes)?;
                Ok(Self::DragOffer { window, path })
            }
            OP_DRAG_WITHDRAW => {
                reserved_zero(bytes, 16)?;
                let window = nonzero_window_id(read_u64(bytes, 8))?;
                Ok(Self::DragWithdraw { window })
            }
            OP_QUERY_DESKTOP => {
                reserved_zero(bytes, 8)?;
                Ok(Self::QueryDesktop)
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Encode the window id + bundle-path payload [`WindowRequest::PinBundle`]
/// and [`WindowRequest::DragOffer`] share verbatim (mirrors
/// [`read_bundle_path`]): the path fills the rest of the fixed frame, so
/// there is no separate reserved tail to zero.
fn encode_bundle_path(out: &mut [u8; WindowRequest::WIRE_LEN], window: u64, path: &BundleRef) {
    put_u64(out, 8, window);
    put_u16(out, 16, path.len);
    out[BUNDLE_PATH_OFFSET..BUNDLE_PATH_OFFSET + WINDOW_BUNDLE_PATH_MAX]
        .copy_from_slice(&path.bytes);
}

/// Decode the window id + [`BundleRef`] payload [`WindowRequest::PinBundle`]
/// and [`WindowRequest::DragOffer`] share verbatim at the same wire offsets:
/// an owning window id followed by a length-prefixed bundle path filling
/// the rest of the fixed frame. The one definition both request arms share,
/// so the path bounds can never diverge between pinning and dragging.
fn read_bundle_path(bytes: &[u8]) -> Result<(u64, BundleRef), Errno> {
    let window = nonzero_window_id(read_u64(bytes, 8))?;
    let path_len = read_u16(bytes, 16);
    let mut path_bytes = [0u8; WINDOW_BUNDLE_PATH_MAX];
    path_bytes
        .copy_from_slice(&bytes[BUNDLE_PATH_OFFSET..BUNDLE_PATH_OFFSET + WINDOW_BUNDLE_PATH_MAX]);
    let path = BundleRef::from_wire(path_len, &path_bytes)?;
    Ok((window, path))
}

/// Decode the operands of a [`WindowRequest::Create`]: the granted region
/// and event route, the frame layout, the title, and the resizability the
/// app asks the window manager for.
///
/// The widest operand block the protocol carries, so it reads as its own
/// step rather than crowding out every other operation in the decoder.
fn read_create(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    reserved_zero(bytes, 42 + WINDOW_TITLE_MAX + 1)?;
    let shm_handle = read_u64(bytes, 8);
    let event_endpoint = read_u64(bytes, 16);
    if crate::ipc::is_reserved_endpoint(event_endpoint) {
        return Err(Errno::OutOfRange);
    }
    let layout = read_frame_layout(bytes)?;
    let mut title_bytes = [0u8; WINDOW_TITLE_MAX];
    title_bytes.copy_from_slice(&bytes[42..42 + WINDOW_TITLE_MAX]);
    let title = WindowTitle::from_wire(bytes[41], &title_bytes)?;
    let resizable = match bytes[42 + WINDOW_TITLE_MAX] {
        0 => false,
        1 => true,
        _ => return Err(Errno::OutOfRange),
    };
    Ok(WindowRequest::Create {
        shm_handle,
        event_endpoint,
        frame_count: layout.frame_count,
        width_px: layout.width_px,
        height_px: layout.height_px,
        stride_bytes: layout.stride_bytes,
        format: layout.format,
        title,
        resizable,
    })
}

/// The frame-layout fields `Create` and `Resize` share verbatim at the same
/// wire offsets: the frame count, geometry, stride, and pixel format.
struct FrameLayout {
    frame_count: u32,
    width_px: u32,
    height_px: u32,
    stride_bytes: u32,
    format: DisplayFormat,
}

/// Decode and validate the frame layout `Create` and `Resize` both carry at
/// bytes 24..=40 — the frame count within `1..=WINDOW_MAX_FRAMES`, a non-zero
/// geometry, a known pixel format, and a stride that holds at least one
/// scanline. The one definition both request arms share, so the geometry
/// bounds can never diverge between opening and resizing a window.
fn read_frame_layout(bytes: &[u8]) -> Result<FrameLayout, Errno> {
    let frame_count = read_u32(bytes, 24);
    if frame_count == 0 || frame_count > WINDOW_MAX_FRAMES {
        return Err(Errno::LengthOutOfRange);
    }
    let width_px = read_u32(bytes, 28);
    let height_px = read_u32(bytes, 32);
    let stride_bytes = read_u32(bytes, 36);
    let format = DisplayFormat::from_u8(bytes[40])?;
    if width_px == 0 || height_px == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    let min_stride = u64::from(width_px) * u64::from(format.bytes_per_pixel());
    if u64::from(stride_bytes) < min_stride {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(FrameLayout {
        frame_count,
        width_px,
        height_px,
        stride_bytes,
        format,
    })
}

/// Refuse a request whose reserved tail (from `from` to the end of the
/// fixed frame) carries any non-zero byte — wire corruption or a smuggled
/// field, never silently ignored.
fn reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..WindowRequest::WIRE_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

/// A window id is minted by the session starting at 1; zero never names a
/// window and is refused rather than looked up.
fn nonzero_window_id(id: u64) -> Result<u64, Errno> {
    if id == 0 {
        return Err(Errno::OutOfRange);
    }
    Ok(id)
}

/// Reply length, in bytes, of a `Create`: the status word, the assigned
/// window id, and the serving session's [`ProcId`].
pub const WINDOW_CREATE_REPLY_LEN: usize = 12 + crate::PROC_ID_LEN;

/// Encode a `Create` outcome: on success the assigned (non-zero) window
/// id followed by the serving session's own [`ProcId`] — the identity an
/// app then requires of every event's kernel-attested sender, closing
/// the event channel against forged input from any other process (the
/// reply itself is trustworthy because the window rendezvous is
/// squat-protected). On refusal, the shared status frame (a negative
/// [`Errno`] discriminant), zero-padded to the same length, so a client
/// always issues one fixed-size receive.
#[must_use]
pub fn encode_create_reply(
    result: Result<u64, Errno>,
    server: ProcId,
) -> [u8; WINDOW_CREATE_REPLY_LEN] {
    let mut out = [0u8; WINDOW_CREATE_REPLY_LEN];
    match result {
        Ok(window_id) => {
            put_u64(&mut out, 4, window_id);
            out[12..].copy_from_slice(server.as_bytes());
        }
        Err(err) => {
            out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err)));
        }
    }
    out
}

/// Reply length, in bytes, of a [`WindowRequest::QueryDesktop`]: the
/// shared status word followed by the [`DesktopInfo`] record.
pub const WINDOW_DESKTOP_REPLY_LEN: usize = 4 + DesktopInfo::WIRE_LEN;

/// Encode a `QueryDesktop` outcome: on success the desktop record after a
/// zero status word; on refusal the shared status frame (a negative
/// [`Errno`] discriminant) zero-padded to the same length, so a client
/// always issues one fixed-size receive.
#[must_use]
pub fn encode_desktop_reply(result: Result<DesktopInfo, Errno>) -> [u8; WINDOW_DESKTOP_REPLY_LEN] {
    let mut out = [0u8; WINDOW_DESKTOP_REPLY_LEN];
    match result {
        Ok(desktop) => desktop.write_to_at(&mut out, 4),
        Err(err) => out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err))),
    }
    out
}

/// Decode a `QueryDesktop` reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word, or a successful reply
///   carrying a desktop no screen or scale could describe.
/// * [`Errno::BadMagic`] — a dirty reserved byte in the record.
/// * The decoded [`Errno`] itself, when the session refused the query.
pub fn decode_desktop_reply(bytes: &[u8]) -> Result<DesktopInfo, Errno> {
    if bytes.len() < WINDOW_DESKTOP_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    DesktopInfo::from_bytes_at(bytes, 4)
}

/// Decode a `Create` reply frame into the assigned window id and the
/// serving session's [`ProcId`].
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word, a successful reply
///   carrying the never-minted zero window id, or the kernel-reserved
///   all-zero server identity (fail closed: an app must never accept an
///   event stream it cannot authenticate).
/// * The decoded [`Errno`] itself, when the session refused the request.
pub fn decode_create_reply(bytes: &[u8]) -> Result<(u64, ProcId), Errno> {
    if bytes.len() < WINDOW_CREATE_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    let window_id = nonzero_window_id(read_u64(bytes, 4))?;
    let server = ProcId::from_bytes(&bytes[12..WINDOW_CREATE_REPLY_LEN])?;
    if server.is_kernel() {
        return Err(Errno::OutOfRange);
    }
    Ok((window_id, server))
}

/// Wire event discriminant of [`WindowEvent::Focus`].
const EV_FOCUS: u16 = 1;
/// Wire event discriminant of [`WindowEvent::Key`].
const EV_KEY: u16 = 2;
/// Wire event discriminant of [`WindowEvent::Pointer`].
const EV_POINTER: u16 = 3;
/// Wire event discriminant of [`WindowEvent::CloseRequested`].
const EV_CLOSE_REQUESTED: u16 = 4;
/// Wire event discriminant of [`WindowEvent::FilePicked`].
const EV_FILE_PICKED: u16 = 5;
/// Wire event discriminant of [`WindowEvent::PickCancelled`].
const EV_PICK_CANCELLED: u16 = 6;
/// Wire event discriminant of [`WindowEvent::Scrolled`].
const EV_SCROLLED: u16 = 7;
/// Wire event discriminant of [`WindowEvent::Minimized`].
const EV_MINIMIZED: u16 = 8;
/// Wire event discriminant of [`WindowEvent::Resized`].
const EV_RESIZED: u16 = 9;
/// Wire event discriminant of [`WindowEvent::RedrawRequested`].
const EV_REDRAW_REQUESTED: u16 = 10;
/// Wire event discriminant of [`WindowEvent::DesktopChanged`].
const EV_DESKTOP_CHANGED: u16 = 11;

/// Wire pointer-action discriminant of [`PointerAction::Moved`].
const PTR_MOVED: u16 = 0;
/// Wire pointer-action discriminant of [`PointerAction::Pressed`].
const PTR_PRESSED: u16 = 1;
/// Wire pointer-action discriminant of [`PointerAction::Released`].
const PTR_RELEASED: u16 = 2;

/// What a routed pointer event did at its window-local position.
///
/// The type makes illegal states unrepresentable: a move carries no
/// button, a press/release exactly one resolved button.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PointerAction {
    /// The pointer moved to the carried position.
    Moved,
    /// A button went down at the carried position.
    Pressed(PointerButtonCode),
    /// A button came up at the carried position.
    Released(PointerButtonCode),
}

/// One window event the session delivers to a window's owning app.
///
/// Events are routed by the session's focus policy: only the app owning
/// the addressed window receives them, and only for windows it created.
/// Pointer positions are **window-local** pixels (origin the window's
/// top-left), already inside the window's extent when the session encodes
/// them; keyboard events reuse the one desktop [`KeyInput`] codec so the
/// key vocabulary has a single definition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WindowEvent {
    /// The window gained (`true`) or lost (`false`) keyboard focus.
    Focus {
        /// The window whose focus changed.
        window_id: u64,
        /// Whether the window now holds focus.
        focused: bool,
    },
    /// A key event routed to the focused window.
    Key {
        /// The focused window.
        window_id: u64,
        /// The key event, exactly as the desktop vocabulary defines it.
        key: KeyInput,
    },
    /// A pointer event at a window-local position.
    Pointer {
        /// The window under the pointer.
        window_id: u64,
        /// Window-local x, in pixels from the window's left edge.
        x: u32,
        /// Window-local y, in pixels from the window's top edge.
        y: u32,
        /// What happened at that position.
        action: PointerAction,
    },
    /// The user asked the session to close the window (title-bar close).
    /// The app owns the decision: it saves, then issues
    /// [`WindowRequest::Close`] — the session never destroys an app's
    /// window behind its back while the app lives.
    CloseRequested {
        /// The window the user asked to close.
        window_id: u64,
    },
    /// The user chose a file in the session's trusted picker
    /// ([`WindowRequest::PickFile`]'s conclusion). `handle` is the
    /// kernel-minted one-shot delegation the app redeems with `fd_redeem`
    /// into a read-only descriptor operated under the *session's* captured
    /// authority — the CU6 user-mediated file capability. The handle is
    /// owner-bound kernel-side, so the value is useless to any other
    /// process.
    FilePicked {
        /// The window whose pick concluded.
        window_id: u64,
        /// The `fd_redeem` handle minted to the app's task; never zero
        /// (the reserved invalid handle).
        handle: u64,
    },
    /// The user dismissed the session's trusted picker without choosing
    /// ([`WindowRequest::PickFile`]'s other conclusion). No authority was
    /// delegated; the app may ask again.
    PickCancelled {
        /// The window whose pick was dismissed.
        window_id: u64,
    },
    /// The window manager minimized the window (the user pressed the
    /// title-bar minimize control, or clicked the taskbar entry): it is
    /// hidden from the workspace but still alive and still listed on the
    /// taskbar. The app may pause non-essential rendering until it is
    /// restored (a later focus/resize event); it need not, and the window
    /// manager destroys nothing behind its back.
    Minimized {
        /// The window that was minimized.
        window_id: u64,
    },
    /// The window manager changed the window's client content size — a
    /// resize-grab that concluded, or a maximize/restore size toggle. The
    /// app re-lays-out to the new size: it allocates a fresh frame region
    /// of `width_px` × `height_px`, re-maps the window onto it
    /// ([`WindowRequest::Resize`]), and presents. The size is the client
    /// content area in pixels (the window-manager furniture is not the
    /// app's to size); it is never zero.
    Resized {
        /// The window whose client size changed.
        window_id: u64,
        /// New client width in pixels; never zero.
        width_px: u32,
        /// New client height in pixels; never zero.
        height_px: u32,
    },
    /// The session released this window's retained content pixels to
    /// reclaim memory, and needs the window presented again.
    ///
    /// The app has lost nothing: its own frame regions, size, title,
    /// furniture, focus and place in the stack are all untouched — only
    /// the session's copy of the pixels went away. Presenting any frame
    /// with full-window damage restores the window exactly as it was.
    ///
    /// A client that ignores the event is not broken: its window simply
    /// shows through to the desktop until it next presents for a reason
    /// of its own. The `tairix-window` client library answers the event
    /// on the app's behalf, so an app only handles it when it wants to
    /// genuinely re-render rather than re-send its last frame.
    RedrawRequested {
        /// The window whose content must be presented again.
        window_id: u64,
    },
    /// The scroll wheel turned over the window while the window owns its
    /// own content scrolling (it exposes no window-manager root viewport,
    /// so the session forwards the ticks to the app instead of consuming
    /// them into furniture). The app applies them to its nested scroll
    /// model exactly as it would a keyboard line step. Ticks are in the
    /// device's detent units: positive `dx` toward the logical end,
    /// positive `dy` downward (the `evdev` orientation), one line step per
    /// tick by convention.
    Scrolled {
        /// The window the pointer was over when the wheel turned.
        window_id: u64,
        /// Signed horizontal scroll ticks.
        dx: i32,
        /// Signed vertical scroll ticks.
        dy: i32,
    },
    /// The desktop this window is displayed on changed: a different screen
    /// extent, a different UI scale, or a switch between the light and
    /// dark appearance ([`WindowRequest::QueryDesktop`] is how an app
    /// learns the state it started from).
    ///
    /// The app re-resolves whatever it derived from the old state — its
    /// scale-dependent metrics, its font sizes, its theme colours — and
    /// presents again. Ignoring the event is not broken: the window simply
    /// keeps the appearance it opened with until the app next re-renders
    /// for a reason of its own.
    ///
    /// The desktop belongs to the seat, not to one window, so the session
    /// sends the event to every live window of every client. A client with
    /// two windows is told twice, and both tell it the same thing.
    DesktopChanged {
        /// The window whose desktop is described.
        window_id: u64,
        /// The desktop as it now is.
        desktop: DesktopInfo,
    },
}

impl WindowEvent {
    /// Encoded size on the wire: magic (4), version (2), kind (2), window
    /// id (8), and a 24-byte event block whose unused tail must be zero
    /// (the embedded [`KeyInput`] record is the widest).
    pub const WIRE_LEN: usize = 40;

    /// The window this event addresses.
    #[must_use]
    pub const fn window_id(&self) -> u64 {
        match *self {
            Self::Focus { window_id, .. }
            | Self::Key { window_id, .. }
            | Self::Pointer { window_id, .. }
            | Self::CloseRequested { window_id }
            | Self::FilePicked { window_id, .. }
            | Self::PickCancelled { window_id }
            | Self::Minimized { window_id }
            | Self::Resized { window_id, .. }
            | Self::RedrawRequested { window_id }
            | Self::Scrolled { window_id, .. }
            | Self::DesktopChanged { window_id, .. } => window_id,
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, WINDOW_EVENT_MAGIC);
        put_u16(&mut out, 4, WINDOW_VERSION_V1);
        put_u64(&mut out, 8, self.window_id());
        match *self {
            Self::Focus { focused, .. } => {
                put_u16(&mut out, 6, EV_FOCUS);
                out[16] = u8::from(focused);
            }
            Self::Key { key, .. } => {
                put_u16(&mut out, 6, EV_KEY);
                out[16..16 + KeyInput::WIRE_LEN].copy_from_slice(&key.to_le_bytes());
            }
            Self::Pointer { x, y, action, .. } => {
                put_u16(&mut out, 6, EV_POINTER);
                put_u32(&mut out, 16, x);
                put_u32(&mut out, 20, y);
                let (kind, button) = match action {
                    PointerAction::Moved => (PTR_MOVED, crate::input::BUTTON_NONE),
                    PointerAction::Pressed(button) => (PTR_PRESSED, button.code()),
                    PointerAction::Released(button) => (PTR_RELEASED, button.code()),
                };
                put_u16(&mut out, 24, kind);
                put_u16(&mut out, 26, button);
            }
            Self::CloseRequested { .. } => {
                put_u16(&mut out, 6, EV_CLOSE_REQUESTED);
            }
            Self::FilePicked { handle, .. } => {
                put_u16(&mut out, 6, EV_FILE_PICKED);
                put_u64(&mut out, 16, handle);
            }
            Self::PickCancelled { .. } => {
                put_u16(&mut out, 6, EV_PICK_CANCELLED);
            }
            Self::Scrolled { dx, dy, .. } => {
                put_u16(&mut out, 6, EV_SCROLLED);
                put_i32(&mut out, 16, dx);
                put_i32(&mut out, 20, dy);
            }
            Self::DesktopChanged { desktop, .. } => {
                put_u16(&mut out, 6, EV_DESKTOP_CHANGED);
                desktop.write_to_at(&mut out, 16);
            }
            Self::Minimized { .. } => {
                put_u16(&mut out, 6, EV_MINIMIZED);
            }
            Self::Resized {
                width_px,
                height_px,
                ..
            } => {
                put_u16(&mut out, 6, EV_RESIZED);
                put_u32(&mut out, 16, width_px);
                put_u32(&mut out, 20, height_px);
            }
            Self::RedrawRequested { .. } => {
                put_u16(&mut out, 6, EV_REDRAW_REQUESTED);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole event.
    /// * [`Errno::BadMagic`] — wrong magic, a dirty reserved tail, or a
    ///   malformed embedded key record.
    /// * [`Errno::AbiVersionUnsupported`] — not `window-v1`.
    /// * [`Errno::OutOfRange`] — an event kind, focus flag, pointer
    ///   action, or button outside the closed set, or a zero window id.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != WINDOW_EVENT_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != WINDOW_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let kind = read_u16(bytes, 6);
        let window_id = nonzero_window_id(read_u64(bytes, 8))?;
        match kind {
            EV_FOCUS => {
                event_reserved_zero(bytes, 17)?;
                let focused = match bytes[16] {
                    0 => false,
                    1 => true,
                    _ => return Err(Errno::OutOfRange),
                };
                Ok(Self::Focus { window_id, focused })
            }
            EV_KEY => {
                event_reserved_zero(bytes, 16 + KeyInput::WIRE_LEN)?;
                let key = KeyInput::from_bytes(&bytes[16..16 + KeyInput::WIRE_LEN])?;
                Ok(Self::Key { window_id, key })
            }
            EV_POINTER => {
                event_reserved_zero(bytes, 28)?;
                let x = read_u32(bytes, 16);
                let y = read_u32(bytes, 20);
                let ptr_kind = read_u16(bytes, 24);
                let button = read_u16(bytes, 26);
                let action = match ptr_kind {
                    PTR_MOVED => {
                        if button != crate::input::BUTTON_NONE {
                            return Err(Errno::OutOfRange);
                        }
                        PointerAction::Moved
                    }
                    PTR_PRESSED => PointerAction::Pressed(PointerButtonCode::from_code(button)?),
                    PTR_RELEASED => PointerAction::Released(PointerButtonCode::from_code(button)?),
                    _ => return Err(Errno::OutOfRange),
                };
                Ok(Self::Pointer {
                    window_id,
                    x,
                    y,
                    action,
                })
            }
            EV_CLOSE_REQUESTED => {
                event_reserved_zero(bytes, 16)?;
                Ok(Self::CloseRequested { window_id })
            }
            EV_FILE_PICKED => {
                event_reserved_zero(bytes, 24)?;
                let handle = read_u64(bytes, 16);
                // Handle 0 is the reserved invalid value the kernel never
                // mints; a "picked" event without a redeemable delegation
                // is refused rather than guessed at.
                if handle == 0 {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::FilePicked { window_id, handle })
            }
            EV_PICK_CANCELLED => {
                event_reserved_zero(bytes, 16)?;
                Ok(Self::PickCancelled { window_id })
            }
            EV_SCROLLED => {
                event_reserved_zero(bytes, 24)?;
                let dx = read_i32(bytes, 16);
                let dy = read_i32(bytes, 20);
                Ok(Self::Scrolled { window_id, dx, dy })
            }
            EV_MINIMIZED => {
                event_reserved_zero(bytes, 16)?;
                Ok(Self::Minimized { window_id })
            }
            EV_RESIZED => {
                event_reserved_zero(bytes, 24)?;
                let width_px = read_u32(bytes, 16);
                let height_px = read_u32(bytes, 20);
                if width_px == 0 || height_px == 0 {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::Resized {
                    window_id,
                    width_px,
                    height_px,
                })
            }
            EV_REDRAW_REQUESTED => {
                event_reserved_zero(bytes, 16)?;
                Ok(Self::RedrawRequested { window_id })
            }
            EV_DESKTOP_CHANGED => {
                event_reserved_zero(bytes, 16 + DesktopInfo::WIRE_LEN)?;
                let desktop = DesktopInfo::from_bytes_at(bytes, 16)?;
                Ok(Self::DesktopChanged { window_id, desktop })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Refuse an event whose reserved tail (from `from` to the end of the
/// fixed frame) carries any non-zero byte.
fn event_reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..WindowEvent::WIRE_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decode_create_reply, decode_desktop_reply, encode_create_reply, encode_desktop_reply,
        BundleRef, PointerAction, WindowEvent, WindowRequest, WindowTitle, WINDOW_BUNDLE_PATH_MAX,
        WINDOW_CREATE_REPLY_LEN, WINDOW_DESKTOP_REPLY_LEN, WINDOW_ENDPOINT, WINDOW_EVENT_MAGIC,
        WINDOW_MAX_FRAMES, WINDOW_REQUEST_MAGIC, WINDOW_TITLE_MAX,
    };
    use crate::desktop::{Appearance, DesktopInfo};
    use crate::driver::display::{DamageRect, DisplayFormat};
    use crate::input::{KeyInput, KeyValue, Modifiers, PointerButtonCode};
    use crate::seat::SEATMGR_ENDPOINT;
    use crate::Errno;
    use crate::ProcId;

    fn sample_create() -> WindowRequest {
        WindowRequest::Create {
            shm_handle: 7,
            event_endpoint: 0x900d,
            frame_count: 2,
            width_px: 320,
            height_px: 200,
            stride_bytes: 1280,
            format: DisplayFormat::Bgra8888,
            title: WindowTitle::new("Files").expect("a valid title"),
            resizable: false,
        }
    }

    fn sample_present() -> WindowRequest {
        WindowRequest::Present {
            window_id: 3,
            frame_index: 1,
            damage: DamageRect {
                x: 10,
                y: 20,
                width_px: 30,
                height_px: 40,
            },
        }
    }

    #[test]
    fn magic_and_endpoint_are_frozen() {
        assert_eq!(WINDOW_REQUEST_MAGIC, u32::from_le_bytes(*b"WIN1"));
        assert_eq!(WINDOW_EVENT_MAGIC, u32::from_le_bytes(*b"WEV1"));
        // "WI" ASCII hex-spelled, the reserved-endpoint convention.
        assert_eq!(WINDOW_ENDPOINT, 0x5749_1001);
        assert!(crate::ipc::is_reserved_endpoint(WINDOW_ENDPOINT));
    }

    #[test]
    fn titles_validate_length_and_content() {
        assert_eq!(WindowTitle::new("").expect("empty is fine").as_str(), "");
        let widest = "w".repeat(WINDOW_TITLE_MAX);
        assert_eq!(
            WindowTitle::new(&widest).expect("max length fits").as_str(),
            widest
        );
        let over = "w".repeat(WINDOW_TITLE_MAX + 1);
        assert_eq!(
            WindowTitle::new(&over).unwrap_err(),
            Errno::LengthOutOfRange
        );
        assert_eq!(
            WindowTitle::new("bad\x1bescape").unwrap_err(),
            Errno::OutOfRange
        );
        assert_eq!(
            WindowTitle::new("two\nlines").unwrap_err(),
            Errno::OutOfRange
        );
    }

    #[test]
    fn bundle_refs_validate_length_and_content() {
        let widest = "p".repeat(WINDOW_BUNDLE_PATH_MAX);
        assert_eq!(
            BundleRef::new(&widest).expect("max length fits").as_str(),
            widest
        );
        assert_eq!(BundleRef::new("").unwrap_err(), Errno::LengthOutOfRange);
        let over = "p".repeat(WINDOW_BUNDLE_PATH_MAX + 1);
        assert_eq!(BundleRef::new(&over).unwrap_err(), Errno::LengthOutOfRange);
        assert_eq!(
            BundleRef::new("/Apps/bad\x1bescape.app").unwrap_err(),
            Errno::OutOfRange
        );
        assert_eq!(
            BundleRef::new("/Apps/two\nlines.app").unwrap_err(),
            Errno::OutOfRange
        );
        assert_eq!(
            BundleRef::new("/Apps/frag#ment.app").unwrap_err(),
            Errno::OutOfRange
        );
    }

    #[test]
    fn requests_round_trip() {
        let widest_path = BundleRef::new(&"p".repeat(WINDOW_BUNDLE_PATH_MAX)).expect("max path");
        for request in [
            sample_create(),
            sample_present(),
            WindowRequest::Close { window_id: 9 },
            WindowRequest::PickFile { window_id: 9 },
            WindowRequest::Resize {
                window_id: 3,
                shm_handle: 11,
                frame_count: 2,
                width_px: 640,
                height_px: 480,
                stride_bytes: 2560,
                format: DisplayFormat::Bgra8888,
            },
            WindowRequest::PinBundle {
                window: 5,
                path: BundleRef::new("/Apps/Editor.app").expect("a valid path"),
            },
            WindowRequest::PinBundle {
                window: 5,
                path: widest_path,
            },
            WindowRequest::DragOffer {
                window: 5,
                path: BundleRef::new("/Apps/Editor.app").expect("a valid path"),
            },
            WindowRequest::DragOffer {
                window: 5,
                path: widest_path,
            },
            WindowRequest::DragWithdraw { window: 5 },
        ] {
            let bytes = request.to_le_bytes();
            assert_eq!(WindowRequest::from_bytes(&bytes), Ok(request));
        }
    }

    #[test]
    fn resize_request_enforces_bounds_and_a_clean_tail() {
        let base = WindowRequest::Resize {
            window_id: 3,
            shm_handle: 11,
            frame_count: 2,
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        };
        // A zero window id is refused.
        let mut zero_id = base.to_le_bytes();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        // A zero / over-large frame count.
        let mut zero_frames = base.to_le_bytes();
        zero_frames[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_frames),
            Err(Errno::LengthOutOfRange)
        );
        // A zero extent, and a stride too small for one scanline.
        let mut zero_w = base.to_le_bytes();
        zero_w[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_w),
            Err(Errno::LengthOutOfRange)
        );
        let mut short_stride = base.to_le_bytes();
        short_stride[36..40].copy_from_slice(&2559u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&short_stride),
            Err(Errno::LengthOutOfRange)
        );
        // A dirty reserved tail (past the format byte at offset 40).
        let mut dirty = base.to_le_bytes();
        dirty[41] = 1;
        assert_eq!(WindowRequest::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn resized_event_refuses_a_zero_extent_and_a_dirty_tail() {
        let base = WindowEvent::Resized {
            window_id: 4,
            width_px: 800,
            height_px: 600,
        };
        let mut zero_w = base.to_le_bytes();
        zero_w[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&zero_w),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_h = base.to_le_bytes();
        zero_h[20..24].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&zero_h),
            Err(Errno::LengthOutOfRange)
        );
        let mut dirty = base.to_le_bytes();
        dirty[24] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
        // Minimized carries no payload past the window id; its tail is dirty-checked.
        let mut minimized = WindowEvent::Minimized { window_id: 4 }.to_le_bytes();
        minimized[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&minimized), Err(Errno::BadMagic));
    }

    #[test]
    fn pick_file_refuses_a_zero_id_and_a_dirty_tail() {
        let mut zero_id = WindowRequest::PickFile { window_id: 9 }.to_le_bytes();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        let mut dirty = WindowRequest::PickFile { window_id: 9 }.to_le_bytes();
        dirty[16] = 1;
        assert_eq!(WindowRequest::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn pin_and_drag_offer_refuse_malformed_ids_and_paths() {
        let sample_path = BundleRef::new("/Apps/Editor.app").expect("a valid path");
        for base in [
            WindowRequest::PinBundle {
                window: 5,
                path: sample_path,
            }
            .to_le_bytes(),
            WindowRequest::DragOffer {
                window: 5,
                path: sample_path,
            }
            .to_le_bytes(),
        ] {
            // A zero window id is refused.
            let mut zero_window = base;
            zero_window[8..16].copy_from_slice(&0u64.to_le_bytes());
            assert_eq!(
                WindowRequest::from_bytes(&zero_window),
                Err(Errno::OutOfRange)
            );

            // A zero path length is refused.
            let mut zero_len = base;
            zero_len[16..18].copy_from_slice(&0u16.to_le_bytes());
            assert_eq!(
                WindowRequest::from_bytes(&zero_len),
                Err(Errno::LengthOutOfRange)
            );

            // A path length past the fixed block is refused.
            let over = u16::try_from(WINDOW_BUNDLE_PATH_MAX + 1).expect("fits a u16");
            let mut over_len = base;
            over_len[16..18].copy_from_slice(&over.to_le_bytes());
            assert_eq!(
                WindowRequest::from_bytes(&over_len),
                Err(Errno::LengthOutOfRange)
            );

            // An embedded control character is refused.
            let mut control_char = base;
            control_char[18] = 0x01;
            assert_eq!(
                WindowRequest::from_bytes(&control_char),
                Err(Errno::OutOfRange)
            );

            // Invalid UTF-8 is refused.
            let mut invalid_utf8 = base;
            invalid_utf8[18] = 0xFF;
            assert_eq!(
                WindowRequest::from_bytes(&invalid_utf8),
                Err(Errno::OutOfRange)
            );

            // A dirty tail past the declared path length is refused.
            let path_len = usize::from(u16::from_le_bytes([base[16], base[17]]));
            let mut dirty_tail = base;
            dirty_tail[18 + path_len] = 1;
            assert_eq!(WindowRequest::from_bytes(&dirty_tail), Err(Errno::BadMagic));
        }
    }

    #[test]
    fn drag_withdraw_refuses_a_zero_id_and_a_dirty_tail() {
        let mut zero_id = WindowRequest::DragWithdraw { window: 9 }.to_le_bytes();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        let mut dirty = WindowRequest::DragWithdraw { window: 9 }.to_le_bytes();
        dirty[16] = 1;
        assert_eq!(WindowRequest::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn decode_fails_closed_on_malformed_framing() {
        let good = sample_create().to_le_bytes();

        assert_eq!(
            WindowRequest::from_bytes(&good[..WindowRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(WindowRequest::from_bytes(&bad_magic), Err(Errno::BadMagic));
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            WindowRequest::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        // Neither the never-allocated zero nor a far-future operation
        // decodes: an unknown op is refused before its payload is read.
        for op in [0u8, 250] {
            let mut bad_op = good;
            bad_op[6] = op;
            assert_eq!(WindowRequest::from_bytes(&bad_op), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn decode_refuses_dirty_reserved_tails() {
        let mut create = sample_create().to_le_bytes();
        create[WindowRequest::WIRE_LEN - 1] = 1;
        assert_eq!(WindowRequest::from_bytes(&create), Err(Errno::BadMagic));
        let mut present = sample_present().to_le_bytes();
        present[36] = 1;
        assert_eq!(WindowRequest::from_bytes(&present), Err(Errno::BadMagic));
        let mut close = WindowRequest::Close { window_id: 9 }.to_le_bytes();
        close[16] = 1;
        assert_eq!(WindowRequest::from_bytes(&close), Err(Errno::BadMagic));
    }

    #[test]
    fn create_bounds_are_enforced() {
        let encode = |frame_count: u32, width: u32, height: u32, stride: u32, format: u8| {
            let mut bytes = sample_create().to_le_bytes();
            bytes[24..28].copy_from_slice(&frame_count.to_le_bytes());
            bytes[28..32].copy_from_slice(&width.to_le_bytes());
            bytes[32..36].copy_from_slice(&height.to_le_bytes());
            bytes[36..40].copy_from_slice(&stride.to_le_bytes());
            bytes[40] = format;
            WindowRequest::from_bytes(&bytes)
        };
        assert_eq!(encode(0, 320, 200, 1280, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(
            encode(WINDOW_MAX_FRAMES + 1, 320, 200, 1280, 2),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(encode(2, 0, 200, 1280, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 320, 0, 1280, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 320, 200, 1279, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 320, 200, 1280, 9), Err(Errno::OutOfRange));
        assert!(encode(WINDOW_MAX_FRAMES, 320, 200, 1280, 2).is_ok());
    }

    #[test]
    fn create_carries_the_resizable_flag_and_rejects_a_dirty_flag_byte() {
        // The flag round-trips both ways.
        let mut resizable = sample_create();
        if let WindowRequest::Create {
            resizable: ref mut flag,
            ..
        } = resizable
        {
            *flag = true;
        }
        let bytes = resizable.to_le_bytes();
        assert_eq!(WindowRequest::from_bytes(&bytes), Ok(resizable));
        // The flag lives at the byte just past the title.
        assert_eq!(bytes[42 + WINDOW_TITLE_MAX], 1);
        assert_eq!(sample_create().to_le_bytes()[42 + WINDOW_TITLE_MAX], 0);
        // A flag byte outside {0, 1} is refused, never coerced.
        let mut bad = sample_create().to_le_bytes();
        bad[42 + WINDOW_TITLE_MAX] = 2;
        assert_eq!(WindowRequest::from_bytes(&bad), Err(Errno::OutOfRange));
    }

    #[test]
    fn create_refuses_a_reserved_event_endpoint() {
        let mut bytes = sample_create().to_le_bytes();
        bytes[16..24].copy_from_slice(&SEATMGR_ENDPOINT.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&bytes), Err(Errno::OutOfRange));
        let mut own = sample_create().to_le_bytes();
        own[16..24].copy_from_slice(&WINDOW_ENDPOINT.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&own), Err(Errno::OutOfRange));
    }

    /// The desktop the query tests round-trip.
    fn sample_desktop() -> DesktopInfo {
        match DesktopInfo::new(1024, 768, 100, Appearance::Dark) {
            Ok(info) => info,
            Err(_) => unreachable!("a 1024x768 screen at 100% is in range"),
        }
    }

    #[test]
    fn the_desktop_query_round_trips_and_names_no_window() {
        let request = WindowRequest::QueryDesktop;
        assert_eq!(
            WindowRequest::from_bytes(&request.to_le_bytes()),
            Ok(request)
        );
        // The one request with no operands: everything past the header is
        // reserved, so a smuggled window id or payload is refused rather
        // than ignored.
        for at in 8..WindowRequest::WIRE_LEN {
            let mut dirty = request.to_le_bytes();
            dirty[at] = 1;
            assert_eq!(WindowRequest::from_bytes(&dirty), Err(Errno::BadMagic));
        }
    }

    #[test]
    fn the_desktop_reply_round_trips_and_fails_closed() {
        let desktop = sample_desktop();
        assert_eq!(
            decode_desktop_reply(&encode_desktop_reply(Ok(desktop))),
            Ok(desktop)
        );
        assert_eq!(
            decode_desktop_reply(&encode_desktop_reply(Err(Errno::PermissionDenied))),
            Err(Errno::PermissionDenied)
        );

        let good = encode_desktop_reply(Ok(desktop));
        assert_eq!(
            decode_desktop_reply(&good[..WINDOW_DESKTOP_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A refusal reply carries no record, and a success reply whose
        // record is blank is not a desktop: neither can be read as one.
        assert!(decode_desktop_reply(&[0u8; WINDOW_DESKTOP_REPLY_LEN]).is_err());
        let mut dirty = good;
        dirty[WINDOW_DESKTOP_REPLY_LEN - 1] = 1;
        assert_eq!(decode_desktop_reply(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn a_desktop_change_event_round_trips_and_fails_closed() {
        let event = WindowEvent::DesktopChanged {
            window_id: 7,
            desktop: sample_desktop(),
        };
        let wire = event.to_le_bytes();
        assert_eq!(WindowEvent::from_bytes(&wire), Ok(event));
        assert_eq!(event.window_id(), 7);

        // The record ends well before the frame does; the tail past it
        // must be zero.
        let mut dirty = wire;
        dirty[WindowEvent::WIRE_LEN - 1] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
        // A malformed record inside a well-formed frame is refused, not
        // clamped to something plausible.
        let mut blank = wire;
        blank[16..16 + DesktopInfo::WIRE_LEN].fill(0);
        assert_eq!(WindowEvent::from_bytes(&blank), Err(Errno::OutOfRange));
    }

    #[test]
    fn create_refuses_a_malformed_title() {
        // An over-long claimed length.
        let mut long = sample_create().to_le_bytes();
        long[41] = u8::try_from(WINDOW_TITLE_MAX + 1).expect("a small test constant");
        assert_eq!(
            WindowRequest::from_bytes(&long),
            Err(Errno::LengthOutOfRange)
        );
        // Bytes past the claimed length must be zero.
        let mut dirty = sample_create().to_le_bytes();
        dirty[42 + 10] = b'x';
        assert_eq!(WindowRequest::from_bytes(&dirty), Err(Errno::BadMagic));
        // Invalid UTF-8 inside the claimed length.
        let mut bad_utf8 = sample_create().to_le_bytes();
        bad_utf8[42] = 0xFF;
        assert_eq!(WindowRequest::from_bytes(&bad_utf8), Err(Errno::OutOfRange));
        // A control character inside the claimed length.
        let mut control = sample_create().to_le_bytes();
        control[42] = 0x1B;
        assert_eq!(WindowRequest::from_bytes(&control), Err(Errno::OutOfRange));
    }

    #[test]
    fn present_refuses_an_empty_damage_rectangle_and_a_zero_id() {
        let mut zero_width = sample_present().to_le_bytes();
        zero_width[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_width),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_height = sample_present().to_le_bytes();
        zero_height[32..36].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_height),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_id = sample_present().to_le_bytes();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        let mut zero_close = WindowRequest::Close { window_id: 9 }.to_le_bytes();
        zero_close[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_close),
            Err(Errno::OutOfRange)
        );
    }

    /// The serving session identity the reply tests stamp.
    fn server() -> ProcId {
        ProcId::from_raw([0x5A; 16])
    }

    #[test]
    fn create_replies_round_trip_ok_and_error() {
        assert_eq!(
            decode_create_reply(&encode_create_reply(Ok(42), server())),
            Ok((42, server()))
        );
        assert_eq!(
            decode_create_reply(&encode_create_reply(Err(Errno::NoSpace), server())),
            Err(Errno::NoSpace)
        );
        assert_eq!(
            decode_create_reply(&encode_create_reply(Err(Errno::PermissionDenied), server())),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn create_reply_decode_fails_closed() {
        let good = encode_create_reply(Ok(42), server());
        assert_eq!(
            decode_create_reply(&good[..WINDOW_CREATE_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A corrupt (positive) status word.
        let mut bad_status = good;
        bad_status[0] = 1;
        assert_eq!(decode_create_reply(&bad_status), Err(Errno::OutOfRange));
        // A "successful" reply carrying the never-minted zero id.
        assert_eq!(
            decode_create_reply(&encode_create_reply(Ok(0), server())),
            Err(Errno::OutOfRange)
        );
        // A "successful" reply carrying the kernel-reserved all-zero
        // server identity: an app must never accept an event stream it
        // cannot authenticate.
        assert_eq!(
            decode_create_reply(&encode_create_reply(Ok(42), ProcId::KERNEL)),
            Err(Errno::OutOfRange)
        );
    }

    fn sample_key() -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char('q'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        }
    }

    #[test]
    fn events_round_trip() {
        for event in [
            WindowEvent::Focus {
                window_id: 4,
                focused: true,
            },
            WindowEvent::Focus {
                window_id: 4,
                focused: false,
            },
            WindowEvent::Key {
                window_id: 4,
                key: sample_key(),
            },
            WindowEvent::Pointer {
                window_id: 4,
                x: 17,
                y: 23,
                action: PointerAction::Moved,
            },
            WindowEvent::Pointer {
                window_id: 4,
                x: 0,
                y: 0,
                action: PointerAction::Pressed(PointerButtonCode::Primary),
            },
            WindowEvent::Pointer {
                window_id: 4,
                x: 1,
                y: 2,
                action: PointerAction::Released(PointerButtonCode::Middle),
            },
            WindowEvent::CloseRequested { window_id: 4 },
            WindowEvent::FilePicked {
                window_id: 4,
                handle: 7,
            },
            WindowEvent::PickCancelled { window_id: 4 },
            WindowEvent::Minimized { window_id: 4 },
            WindowEvent::Resized {
                window_id: 4,
                width_px: 800,
                height_px: 600,
            },
            WindowEvent::Scrolled {
                window_id: 4,
                dx: 0,
                dy: 3,
            },
            WindowEvent::Scrolled {
                window_id: 4,
                dx: -2,
                dy: -5,
            },
            WindowEvent::RedrawRequested { window_id: 4 },
        ] {
            let bytes = event.to_le_bytes();
            assert_eq!(WindowEvent::from_bytes(&bytes), Ok(event));
            assert_eq!(event.window_id(), 4);
        }
    }

    #[test]
    fn scroll_events_carry_signed_ticks_and_fail_closed_on_a_dirty_tail() {
        let event = WindowEvent::Scrolled {
            window_id: 9,
            dx: -7,
            dy: 11,
        };
        let bytes = event.to_le_bytes();
        assert_eq!(WindowEvent::from_bytes(&bytes), Ok(event));
        // The 8 bytes past the two i32 ticks are reserved and must be zero.
        let mut dirty = bytes;
        dirty[24] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn pick_events_fail_closed_on_a_zero_handle_and_dirty_tails() {
        // A "picked" event must carry a redeemable (non-zero) handle.
        let mut zero_handle = WindowEvent::FilePicked {
            window_id: 4,
            handle: 7,
        }
        .to_le_bytes();
        zero_handle[16..24].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&zero_handle),
            Err(Errno::OutOfRange)
        );
        // Reserved tails must be zero for both conclusions.
        let mut picked = WindowEvent::FilePicked {
            window_id: 4,
            handle: 7,
        }
        .to_le_bytes();
        picked[24] = 1;
        assert_eq!(WindowEvent::from_bytes(&picked), Err(Errno::BadMagic));
        let mut cancelled = WindowEvent::PickCancelled { window_id: 4 }.to_le_bytes();
        cancelled[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&cancelled), Err(Errno::BadMagic));
    }

    #[test]
    fn event_decode_fails_closed_on_malformed_framing() {
        let good = WindowEvent::CloseRequested { window_id: 4 }.to_le_bytes();

        assert_eq!(
            WindowEvent::from_bytes(&good[..WindowEvent::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(WindowEvent::from_bytes(&bad_magic), Err(Errno::BadMagic));
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            WindowEvent::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        let mut bad_kind = good;
        bad_kind[6] = 99;
        assert_eq!(WindowEvent::from_bytes(&bad_kind), Err(Errno::OutOfRange));
        let mut zero_id = good;
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowEvent::from_bytes(&zero_id), Err(Errno::OutOfRange));
    }

    #[test]
    fn event_decode_refuses_dirty_reserved_tails() {
        let mut focus = WindowEvent::Focus {
            window_id: 4,
            focused: true,
        }
        .to_le_bytes();
        focus[17] = 1;
        assert_eq!(WindowEvent::from_bytes(&focus), Err(Errno::BadMagic));
        let mut key = WindowEvent::Key {
            window_id: 4,
            key: sample_key(),
        }
        .to_le_bytes();
        key[WindowEvent::WIRE_LEN - 1] = 1;
        assert_eq!(WindowEvent::from_bytes(&key), Err(Errno::BadMagic));
        let mut pointer = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
        }
        .to_le_bytes();
        pointer[28] = 1;
        assert_eq!(WindowEvent::from_bytes(&pointer), Err(Errno::BadMagic));
        let mut close = WindowEvent::CloseRequested { window_id: 4 }.to_le_bytes();
        close[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&close), Err(Errno::BadMagic));
        let mut redraw = WindowEvent::RedrawRequested { window_id: 4 }.to_le_bytes();
        redraw[WindowEvent::WIRE_LEN - 1] = 1;
        assert_eq!(WindowEvent::from_bytes(&redraw), Err(Errno::BadMagic));
    }

    #[test]
    fn event_decode_refuses_inconsistent_payloads() {
        // A focus flag outside {0, 1}.
        let mut focus = WindowEvent::Focus {
            window_id: 4,
            focused: true,
        }
        .to_le_bytes();
        focus[16] = 2;
        assert_eq!(WindowEvent::from_bytes(&focus), Err(Errno::OutOfRange));
        // A malformed embedded key record (bad key magic).
        let mut key = WindowEvent::Key {
            window_id: 4,
            key: sample_key(),
        }
        .to_le_bytes();
        key[16] ^= 0xFF;
        assert_eq!(WindowEvent::from_bytes(&key), Err(Errno::BadMagic));
        // A button on a move.
        let mut moved = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
        }
        .to_le_bytes();
        moved[26] = 1;
        assert_eq!(WindowEvent::from_bytes(&moved), Err(Errno::OutOfRange));
        // No button on a press, and an unknown pointer action.
        let mut pressed = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Pressed(PointerButtonCode::Primary),
        }
        .to_le_bytes();
        pressed[26] = 0;
        assert_eq!(WindowEvent::from_bytes(&pressed), Err(Errno::OutOfRange));
        let mut bad_action = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
        }
        .to_le_bytes();
        bad_action[24] = 9;
        assert_eq!(WindowEvent::from_bytes(&bad_action), Err(Errno::OutOfRange));
    }
}
