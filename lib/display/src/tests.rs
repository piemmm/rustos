//! Host tests: the server engine against mock seams, and the client
//! halves wired to a real [`DisplayServer`] through a loopback
//! transport, so both halves are proven against the one shared
//! definition of the protocol semantics.

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::display_ipc::{DisplayRequest, DISPLAY_MAX_FRAMES};
use tairix_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{DriverError, Errno};

use crate::client::{DisplayClient, DisplayTransport, RemoteDisplay};
use crate::driver_error_from_errno;
use crate::server::{DisplayServer, FrameRegion, SeatCheck, ShmMapper, DISPLAY_REPLY_MAX};

/// 4×3 BGRA test mode, stride == one scanline.
const MODE: DisplayMode = DisplayMode {
    width_px: 4,
    height_px: 3,
    stride_bytes: 16,
    format: DisplayFormat::Bgra8888,
};

/// Bytes one MODE frame occupies.
const FRAME_LEN: usize = 48;

const SEAT: u64 = 0;
const TICKET: u64 = 7;

/// A seat oracle scripted per test: `Ok(generation)` or a typed refusal.
struct MockSeat {
    answer: Result<u64, Errno>,
    asked: Vec<(u64, u64)>,
}

impl MockSeat {
    fn live(generation: u64) -> Self {
        Self {
            answer: Ok(generation),
            asked: Vec::new(),
        }
    }

    fn refusing(err: Errno) -> Self {
        Self {
            answer: Err(err),
            asked: Vec::new(),
        }
    }
}

impl SeatCheck for MockSeat {
    fn live_generation(&mut self, ticket: u64, seat_id: u64) -> Result<u64, Errno> {
        self.asked.push((ticket, seat_id));
        self.answer
    }
}

/// The shared backing store standing in for one shm region: tests write
/// it and the mapper snapshots it at map time. (Real shared memory is a
/// live aliased mapping; the engine only reads the region at present
/// time, so a map-time snapshot exercises the same code path — tests
/// fill their frame content before configuring.)
type SharedBytes = Rc<RefCell<Vec<u8>>>;

struct MockRegion {
    bytes: Vec<u8>,
}

impl FrameRegion for MockRegion {
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A mapper that knows one grant handle and its shared backing.
struct MockMapper {
    handle: u64,
    bytes: SharedBytes,
    maps: Rc<RefCell<u32>>,
}

impl ShmMapper for MockMapper {
    type Region = MockRegion;

    fn map(&mut self, handle: u64, min_len: usize) -> Result<Self::Region, Errno> {
        if handle != self.handle {
            return Err(Errno::NotFound);
        }
        if self.bytes.borrow().len() < min_len {
            return Err(Errno::LengthOutOfRange);
        }
        *self.maps.borrow_mut() += 1;
        Ok(MockRegion {
            bytes: self.bytes.borrow().clone(),
        })
    }
}

/// A display that records what reached scan-out.
#[derive(Default)]
struct RecordingDisplay {
    scanout: Vec<u8>,
    presents: u32,
    region_presents: u32,
    last_damage: Option<DamageRect>,
    fail_with: Option<DriverError>,
}

impl RecordingDisplay {
    fn new() -> Self {
        Self {
            scanout: vec![0u8; FRAME_LEN],
            ..Self::default()
        }
    }
}

impl Display for RecordingDisplay {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(MODE)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if let Some(err) = self.fail_with {
            return Err(err);
        }
        if frame.len() < FRAME_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        self.scanout.copy_from_slice(&frame[..FRAME_LEN]);
        self.presents += 1;
        self.last_damage = None;
        Ok(())
    }

    fn present_region(&mut self, frame: &[u8], damage: DamageRect) -> Result<(), DriverError> {
        if let Some(err) = self.fail_with {
            return Err(err);
        }
        damage.validate_in(&MODE)?;
        if frame.len() < FRAME_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        let stride = MODE.stride_bytes as usize;
        let x0 = damage.x as usize * 4;
        let span = damage.width_px as usize * 4;
        for row in 0..damage.height_px as usize {
            let line = (damage.y as usize + row) * stride + x0;
            self.scanout[line..line + span].copy_from_slice(&frame[line..line + span]);
        }
        self.region_presents += 1;
        self.last_damage = Some(damage);
        Ok(())
    }
}

const GRANT: u64 = 42;

/// A server rig: engine + display + seat oracle + the shared region.
struct Rig {
    server: DisplayServer<MockMapper>,
    display: RecordingDisplay,
    seat: MockSeat,
    bytes: SharedBytes,
    maps: Rc<RefCell<u32>>,
}

impl Rig {
    fn new(frames: u32, generation: u64) -> Self {
        let bytes: SharedBytes = Rc::new(RefCell::new(vec![0u8; FRAME_LEN * frames as usize]));
        let maps = Rc::new(RefCell::new(0));
        Self {
            server: DisplayServer::new(MockMapper {
                handle: GRANT,
                bytes: Rc::clone(&bytes),
                maps: Rc::clone(&maps),
            }),
            display: RecordingDisplay::new(),
            seat: MockSeat::live(generation),
            bytes,
            maps,
        }
    }

    fn serve(&mut self, request: &DisplayRequest) -> Vec<u8> {
        let mut reply = [0u8; DISPLAY_REPLY_MAX];
        let len = self.server.serve(
            &mut self.display,
            &mut self.seat,
            TICKET,
            &request.to_le_bytes(),
            &mut reply,
        );
        reply[..len].to_vec()
    }

    fn status(&mut self, request: &DisplayRequest) -> Result<(), Errno> {
        let reply = self.serve(request);
        decode_status_reply(&reply)
    }

    fn configure(&mut self, frames: u32) -> Result<(), Errno> {
        self.status(&DisplayRequest::Configure {
            seat_id: SEAT,
            shm_handle: GRANT,
            frame_count: frames,
            width_px: MODE.width_px,
            height_px: MODE.height_px,
            stride_bytes: MODE.stride_bytes,
            format: MODE.format,
        })
    }

    fn present(&mut self, frame_index: u32, damage: DamageRect) -> Result<(), Errno> {
        self.status(&DisplayRequest::Present {
            seat_id: SEAT,
            frame_index,
            damage,
        })
    }
}

fn full() -> DamageRect {
    DamageRect::full(&MODE)
}

// --- server ---------------------------------------------------------

#[test]
fn query_returns_the_mode_to_the_live_owner_only() {
    let mut rig = Rig::new(2, 1);
    let reply = rig.serve(&DisplayRequest::Query { seat_id: SEAT });
    assert_eq!(tairix_abi::display_ipc::decode_mode_reply(&reply), Ok(MODE));
    assert_eq!(rig.seat.asked, vec![(TICKET, SEAT)]);

    // A non-owner learns nothing — not even the mode.
    rig.seat = MockSeat::refusing(Errno::SeatNotOwner);
    let reply = rig.serve(&DisplayRequest::Query { seat_id: SEAT });
    assert_eq!(
        tairix_abi::display_ipc::decode_mode_reply(&reply),
        Err(Errno::SeatNotOwner)
    );
}

#[test]
fn a_malformed_request_is_refused_before_the_seat_is_asked() {
    let mut rig = Rig::new(2, 1);
    let mut reply = [0u8; DISPLAY_REPLY_MAX];
    let len = rig.server.serve(
        &mut rig.display,
        &mut rig.seat,
        TICKET,
        &[0u8; 4],
        &mut reply,
    );
    assert_eq!(
        decode_status_reply(&reply[..len]),
        Err(Errno::BufferTooSmall)
    );
    assert!(rig.seat.asked.is_empty(), "no oracle call for garbage");
}

#[test]
fn configure_maps_once_and_present_blits_the_indexed_frame() {
    let mut rig = Rig::new(2, 1);
    // Render frame 1 in the shared region, then hand it over.
    rig.bytes.borrow_mut()[FRAME_LEN..].fill(0xAB);
    assert_eq!(rig.configure(2), Ok(()));
    assert!(rig.server.is_configured());
    assert_eq!(*rig.maps.borrow(), 1, "the region is mapped exactly once");

    assert_eq!(rig.present(1, full()), Ok(()));
    assert_eq!(rig.display.presents, 1, "full damage takes the full blit");
    assert_eq!(rig.display.scanout, vec![0xAB; FRAME_LEN]);
    assert_eq!(*rig.maps.borrow(), 1, "no mapping on the present hot path");
}

#[test]
fn present_with_partial_damage_blits_only_the_region() {
    let mut rig = Rig::new(1, 1);
    rig.bytes.borrow_mut().fill(0xCD);
    assert_eq!(rig.configure(1), Ok(()));
    let damage = DamageRect {
        x: 1,
        y: 1,
        width_px: 2,
        height_px: 1,
    };
    assert_eq!(rig.present(0, damage), Ok(()));
    assert_eq!(rig.display.region_presents, 1);
    assert_eq!(rig.display.last_damage, Some(damage));
    // Only the damaged span reached scan-out.
    let mut want = vec![0u8; FRAME_LEN];
    want[16 + 4..16 + 12].fill(0xCD);
    assert_eq!(rig.display.scanout, want);
}

#[test]
fn configure_refuses_a_geometry_that_is_not_the_active_mode() {
    let mut rig = Rig::new(2, 1);
    for (w, h, stride, format) in [
        (
            MODE.width_px + 1,
            MODE.height_px,
            MODE.stride_bytes,
            MODE.format,
        ),
        (
            MODE.width_px,
            MODE.height_px + 1,
            MODE.stride_bytes,
            MODE.format,
        ),
        (
            MODE.width_px,
            MODE.height_px,
            MODE.stride_bytes + 16,
            MODE.format,
        ),
        (
            MODE.width_px,
            MODE.height_px,
            MODE.stride_bytes,
            DisplayFormat::Rgba8888,
        ),
    ] {
        let refused = rig.status(&DisplayRequest::Configure {
            seat_id: SEAT,
            shm_handle: GRANT,
            frame_count: 1,
            width_px: w,
            height_px: h,
            stride_bytes: stride,
            format,
        });
        assert_eq!(refused, Err(Errno::LengthOutOfRange));
        assert!(!rig.server.is_configured());
    }
}

#[test]
fn configure_refuses_an_unknown_grant_and_a_short_region() {
    let mut rig = Rig::new(2, 1);
    let unknown = rig.status(&DisplayRequest::Configure {
        seat_id: SEAT,
        shm_handle: GRANT + 1,
        frame_count: 2,
        width_px: MODE.width_px,
        height_px: MODE.height_px,
        stride_bytes: MODE.stride_bytes,
        format: MODE.format,
    });
    assert_eq!(unknown, Err(Errno::NotFound));

    // A region sized for two frames cannot hold four.
    assert_eq!(rig.configure(4), Err(Errno::LengthOutOfRange));
    assert!(!rig.server.is_configured());
}

#[test]
fn present_is_refused_without_before_and_out_of_bounds_configuration() {
    let mut rig = Rig::new(2, 1);
    // No configuration yet.
    assert_eq!(rig.present(0, full()), Err(Errno::NotFound));
    assert_eq!(rig.configure(2), Ok(()));
    // Frame index beyond the configured count.
    assert_eq!(rig.present(2, full()), Err(Errno::OutOfRange));
    // Damage escaping the mode.
    let escape = DamageRect {
        x: 3,
        y: 0,
        width_px: 2,
        height_px: 1,
    };
    assert_eq!(rig.present(0, escape), Err(Errno::LengthOutOfRange));
    assert_eq!(rig.display.presents + rig.display.region_presents, 0);
}

#[test]
fn a_present_under_a_newer_lease_requires_reconfigure() {
    let mut rig = Rig::new(2, 1);
    assert_eq!(rig.configure(2), Ok(()));
    // The seat was released and re-acquired: same owner task id in the
    // rig, but a newer generation.
    rig.seat = MockSeat::live(2);
    assert_eq!(rig.present(0, full()), Err(Errno::NotFound));
    // Reconfiguring under the live lease restores presentability.
    assert_eq!(rig.configure(2), Ok(()));
    assert_eq!(rig.present(0, full()), Ok(()));
}

#[test]
fn losing_the_lease_drops_the_configuration_and_refuses_typed() {
    let mut rig = Rig::new(2, 1);
    assert_eq!(rig.configure(2), Ok(()));
    rig.seat = MockSeat::refusing(Errno::SeatRevoked);
    assert_eq!(rig.present(0, full()), Err(Errno::SeatRevoked));
    assert!(
        !rig.server.is_configured(),
        "a revoked owner's frames are released, never scanned out"
    );
}

#[test]
fn driver_failures_surface_as_typed_errnos() {
    let mut rig = Rig::new(1, 1);
    assert_eq!(rig.configure(1), Ok(()));
    rig.display.fail_with = Some(DriverError::DeviceFault);
    assert_eq!(rig.present(0, full()), Err(Errno::DeviceFault));
    rig.display.fail_with = Some(DriverError::Busy);
    assert_eq!(rig.present(0, full()), Err(Errno::WouldBlock));
}

// --- error conversions ----------------------------------------------

#[test]
fn error_conversions_preserve_the_seat_and_fault_vocabulary() {
    assert_eq!(
        driver_error_from_errno(Errno::SeatRevoked),
        DriverError::SeatRevoked
    );
    assert_eq!(
        driver_error_from_errno(Errno::SeatNotOwner),
        DriverError::PermissionDenied
    );
    // A condition with no driver equivalent fails closed as a fault.
    assert_eq!(
        driver_error_from_errno(Errno::EntropyNotReady),
        DriverError::DeviceFault
    );
}

// --- client + server end to end --------------------------------------

/// A loopback transport: each call runs one serve pass of a real
/// [`DisplayServer`] over the shared rig.
struct Loopback {
    rig: Rc<RefCell<Rig>>,
}

impl DisplayTransport for Loopback {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let mut rig = self.rig.borrow_mut();
        let mut buf = [0u8; DISPLAY_REPLY_MAX];
        let Rig {
            server,
            display,
            seat,
            ..
        } = &mut *rig;
        let len = server.serve(display, seat, TICKET, request, &mut buf);
        reply[..len].copy_from_slice(&buf[..len]);
        Ok(len)
    }
}

/// Bring up a configured client session over a loopback rig.
fn client_session(frames: u32) -> (Rc<RefCell<Rig>>, DisplayClient<Loopback>, DisplayMode) {
    let rig = Rc::new(RefCell::new(Rig::new(frames, 1)));
    let mut client = DisplayClient::new(
        Loopback {
            rig: Rc::clone(&rig),
        },
        SEAT,
    );
    let mode = client.query().expect("owner queries the mode");
    client
        .configure(GRANT, frames, &mode)
        .expect("configure under the live lease");
    (rig, client, mode)
}

#[test]
fn remote_display_round_trips_a_frame_to_scanout() {
    let (rig, client, mode) = client_session(2);
    // The client's own view of the shared region.
    let mut view = vec![0u8; FRAME_LEN * 2];
    // Compositor-side full frame.
    let frame = vec![0x5A; FRAME_LEN];

    let mut remote = RemoteDisplay::new(client, mode, &mut view, 2).expect("valid session");
    remote.present(&frame).expect("present succeeds");
    drop(remote);
    // The client wrote frame 0 of its view. (In production the view and
    // the server's mapping alias one shm region; the mock keeps them
    // separate, so the server side is asserted through the recorded
    // protocol activity.)
    assert_eq!(view[..FRAME_LEN], frame[..]);
    let rig = rig.borrow();
    assert_eq!(
        rig.display.presents, 1,
        "full-frame damage takes the full blit"
    );
}

#[test]
fn remote_display_tracks_stale_regions_across_the_ring() {
    let (rig, client, mode) = client_session(2);
    let mut view = vec![0u8; FRAME_LEN * 2];
    let mut remote = RemoteDisplay::new(client, mode, &mut view, 2).expect("valid session");

    // First present: whole surface (frame 0 starts wholly stale).
    let frame_a = vec![0x11; FRAME_LEN];
    remote.present(&frame_a).expect("first present");

    // Second present with a one-row damage into frame 1: frame 1 was
    // never written, so the copy must refresh the whole frame (stale ∪
    // damage), leaving no zero bytes from the ring buffer's past.
    let mut frame_b = frame_a.clone();
    for x in 0..4 {
        frame_b[16 + x * 4..16 + x * 4 + 4].fill(0x22);
    }
    let damage = DamageRect {
        x: 0,
        y: 1,
        width_px: 4,
        height_px: 1,
    };
    remote
        .present_region(&frame_b, damage)
        .expect("damage present");

    // Third present into frame 0 with the same damage: frame 0 missed
    // frame_b's row, so the union refreshes it too.
    let frame_c = frame_b.clone();
    remote
        .present_region(&frame_c, damage)
        .expect("second damage present");
    drop(remote);

    // Frame 1 was made fully current by present #2 (stale ∪ damage
    // covered the whole frame) and untouched since; frame 0 caught up
    // on present #3 through its accumulated stale region.
    assert_eq!(view[FRAME_LEN..], frame_b[..], "frame 1 is fully current");
    assert_eq!(view[..FRAME_LEN], frame_c[..], "frame 0 caught up");

    let rig = rig.borrow();
    assert_eq!(rig.display.region_presents, 2);
    assert_eq!(rig.display.last_damage, Some(damage));
}

#[test]
fn remote_display_validates_its_construction_and_inputs() {
    let (_rig, client, mode) = client_session(2);
    let mut short = vec![0u8; FRAME_LEN];
    assert_eq!(
        RemoteDisplay::new(client, mode, &mut short, 2).err(),
        Some(Errno::LengthOutOfRange),
        "a view too small for the ring is refused"
    );

    let (_rig, client, mode) = client_session(2);
    let mut view = vec![0u8; FRAME_LEN * 2];
    assert_eq!(
        RemoteDisplay::new(client, mode, &mut view, DISPLAY_MAX_FRAMES + 1).err(),
        Some(Errno::LengthOutOfRange),
        "the frame-count bound holds client-side too"
    );

    let (_rig, client, mode) = client_session(2);
    let mut view = vec![0u8; FRAME_LEN * 2];
    let mut remote = RemoteDisplay::new(client, mode, &mut view, 2).expect("valid session");
    let escape = DamageRect {
        x: 3,
        y: 0,
        width_px: 2,
        height_px: 1,
    };
    assert_eq!(
        remote.present_region(&[0u8; FRAME_LEN], escape),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        remote.present(&[0u8; 4]),
        Err(DriverError::BufferTooSmall),
        "a short frame is refused before any copy"
    );
}

#[test]
fn a_revoked_client_sees_the_typed_teardown_signal() {
    let (rig, client, mode) = client_session(2);
    let mut view = vec![0u8; FRAME_LEN * 2];
    let mut remote = RemoteDisplay::new(client, mode, &mut view, 2).expect("valid session");
    rig.borrow_mut().seat = MockSeat::refusing(Errno::SeatRevoked);
    assert_eq!(
        remote.present(&[0x33; FRAME_LEN]),
        Err(DriverError::SeatRevoked),
        "the compositor's present surfaces the distinct revocation"
    );
}
