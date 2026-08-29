//! Deterministic fuzz-style integration test for the ABI decoders.
//!
//! Every decoder in `lib/abi` accepts an arbitrary byte slice from a
//! possibly hostile peer; the right way to drive it is
//! a fuzz harness. This file is the smoke harness that runs in
//! `cargo test`: a deterministic 64-bit LCG generates 100 000 short
//! pseudo-random inputs and asserts the decoders refuse them cleanly
//! without panicking and without ever producing an `Ok` result that
//! disagrees with the round-trip encoder.
//!
//! The same set of decoder functions is the entry point the `cargo xtask
//! fuzz` orchestrator drives for its wall-clock budget per PR; the helper [`exercise`] keeps the contract centralised so the two
//! cannot drift.
//!
//! ## Seed and budget
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `tairix_fuzzseed` seam (one definition). A plain `cargo test`
//! runs the fixed [`SMOKE_ITERATIONS`] sweep **once** from a *fresh, logged*
//! seed; `cargo xtask fuzz --soak` sets `TAIRIX_FUZZ_BUDGET_SECS` and the
//! PRNG-driven harness keeps drawing inputs from the *same continuing* stream
//! until the deadline elapses. The seed is logged at the start of the run (and
//! pinnable via `--seed`/`TAIRIX_FUZZ_SEED`), so a fresh-seed crash is still
//! reproducible. The bit-flip harness is an exhaustive boundary sweep, not a
//! random one, so it does not draw a seed.

use tairix_abi::appdata_ipc::{
    decode_blob_list_reply, decode_document_reply, decode_grant_reply, decode_quota_reply,
    decode_temp_reply, encode_blob_list_reply, encode_document_reply, encode_grant_reply,
    encode_quota_reply, encode_temp_reply, AppDataRequest, BlobListing, ConfigDocument,
    APPDATA_BLOB_ENTRY_LEN, APPDATA_BLOB_LIST_MAX, APPDATA_DOCUMENT_MAX, APPDATA_GRANT_REPLY_LEN,
    APPDATA_MAX_REPLY, APPDATA_MAX_REQUEST, APPDATA_QUOTA_REPLY_LEN, APPDATA_TEMP_REPLY_LEN,
};
use tairix_abi::display_ipc::{decode_mode_reply, DisplayRequest};
use tairix_abi::driver::display::{DamageRect, DisplayFormat};
use tairix_abi::driver::net_channel::{
    decode_facts_reply, decode_service_reply, NetChannelNotify, NetChannelRequest,
};
use tairix_abi::elevate::{ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN};
use tairix_abi::font_ipc::{
    decode_families_reply, decode_glyph_reply, decode_metrics_reply, encode_families_reply,
    encode_glyph_reply, FamilyKey, FontRequest, FONT_MAX_FAMILIES_REPLY, FONT_MAX_GLYPH_REPLY,
};
use tairix_abi::fs::{DirEntries, DirEntry, FileKind, FileStat, OpenFlags, FS_NAME_MAX};
use tairix_abi::input::{KeyInput, PointerInput};
use tairix_abi::net::{
    decode_bind_reply, decode_send_reply, decode_socket_reply, SocketDatagram, SocketRequest,
    SocketStreamEvent,
};
use tairix_abi::notify_ipc::{NotifyBody, NotifyRequest, NotifySeverity, NotifyTitle};
use tairix_abi::pinboard_ipc::{PinboardDocument, PinboardRequest};
use tairix_abi::power::PowerAction;
use tairix_abi::process::{ProcessStart, ProcessStartHeader, StringSlot};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::rlimit::ResourceLimit;
use tairix_abi::seat::SeatAdminRequest;
use tairix_abi::service_control::{
    decode_reply as decode_service_control_reply, ServiceControlRequest,
    REQUEST_LEN as SERVICE_CONTROL_REQUEST_LEN,
};
use tairix_abi::session_ipc::{
    decode_account_page, encode_account_page, SessionRequest, SessionVerdict, SESSION_MAX_REPLY,
    SESSION_MAX_REQUEST, SESSION_VERDICT_LEN,
};
use tairix_abi::switchboard_ipc::{
    decode_publish_reply, CommandSection, FrameReport, SeatReport, SwitchboardCommand,
    SwitchboardRequest, TrayPermille, TrayPressure, TrayPressureCount, TrayPressureKind,
    TraySummary, TrayTask, TrayTaskName,
};
use tairix_abi::sysinfo::{
    decode_reply, encode_reply_ok, fold_cache_ledgers, CacheLedgerListRequest, CacheLedgerRecord,
    CacheReportRequest, CpuLoadRecord, CpuLoadRequest, DesktopFrameRecord,
    DesktopFrameStatsRequest, DesktopFrameTotals, IntrospectDomain, KernelMemoryStats,
    MemoryPressureStats, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
    RamzipStats, ReclaimClassRecord, ReclaimListRequest, ResourceLimitRecord, SeatListRequest,
    SeatRecord, SysinfoRequestHeader, SystemIdentity, Uptime, SYSINFO_REPLY_STATUS_LEN,
};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::users_admin::{
    decode_group_list, decode_user_list, UsersAdminRequest, USERS_ADMIN_MAX_REQUEST,
};
use tairix_abi::window_ipc::{
    decode_create_reply, decode_desktop_reply, AppBar, AppMenu, AppMenuItemId, AppMenuLabel,
    AppMenuMark, AppMenuRow, WindowEvent, WindowRequest, WindowSizing, WindowTitle,
};
use tairix_abi::{
    AppInfoHeader, IpcMessageHeader, LoadImage, ManifestHeader, NeededLibrary, Origin, PortName,
    ReadyCondition, ServiceLimit, ServiceManifest, ServiceUnit, PUBLISHER_CERT_CONTEXT,
    PUBLISHER_ID_CONTEXT, SYSCALL_TABLE_HASH_LEN,
};

/// Fixed CFI tag fed to [`LoadImage::parse`] in the harness. A random input
/// is overwhelmingly unlikely to match it, so the loader fails closed long
/// before mapping anything; the point is that no input panics.
const FUZZ_CFI_TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0u8; SYSCALL_TABLE_HASH_LEN];

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Drive the elevation-protocol decoders on `bytes` (one arm of
/// [`exercise`]): an accepted request/reply must round-trip through its
/// encoder; everything else must refuse cleanly, never panic.
fn exercise_elevate(bytes: &[u8]) {
    if let Ok(request) = ElevateRequest::decode(bytes) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = request
            .encode(&mut buf)
            .expect("round-trip encode of an accepted request must succeed");
        let redecoded = ElevateRequest::decode(&buf[..len])
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(reply) = ElevateReply::decode(bytes) {
        let mut buf = [0u8; ELEVATE_REPLY_LEN];
        let len = reply
            .encode(&mut buf)
            .expect("round-trip encode of an accepted reply must succeed");
        let redecoded = ElevateReply::decode(&buf[..len])
            .expect("round-trip of an accepted reply must succeed");
        assert_eq!(reply, redecoded);
    }
}

/// Drive the `session-v1` graphical-login decoders on `bytes` (one arm of
/// [`exercise`]): the request, the account page, and the verdict each
/// round-trip through their encoder; everything else refuses cleanly.
fn exercise_session_ipc(bytes: &[u8]) {
    if let Ok(request) = SessionRequest::decode(bytes) {
        let mut buf = [0u8; SESSION_MAX_REQUEST];
        let len = request
            .encode(&mut buf)
            .expect("round-trip encode of an accepted request must succeed");
        let redecoded = SessionRequest::decode(&buf[..len])
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(page) = decode_account_page(bytes) {
        let mut buf = [0u8; SESSION_MAX_REPLY];
        let len = encode_account_page(&mut buf, page.total(), page.offset(), page.accounts())
            .expect("round-trip encode of an accepted page must succeed");
        let redecoded =
            decode_account_page(&buf[..len]).expect("round-trip of an accepted page must succeed");
        assert_eq!(page, redecoded);
    }
    if let Ok(verdict) = SessionVerdict::decode(bytes) {
        let mut buf = [0u8; SESSION_VERDICT_LEN];
        let len = verdict
            .encode(&mut buf)
            .expect("round-trip encode of an accepted verdict must succeed");
        let redecoded = SessionVerdict::decode(&buf[..len])
            .expect("round-trip of an accepted verdict must succeed");
        assert_eq!(verdict, redecoded);
    }
}

/// Drive the `users_admin` decoders on `bytes` (one arm of [`exercise`]):
/// the typed request record round-trips through its encoder, and walking
/// every list-response entry must refuse cleanly, never panic (a
/// malformed entry ends the iteration fail-closed).
fn exercise_users_admin(bytes: &[u8]) {
    if let Ok(request) = UsersAdminRequest::decode(bytes) {
        let mut buf = [0u8; USERS_ADMIN_MAX_REQUEST];
        let len = request
            .encode_into(&mut buf)
            .expect("round-trip encode of an accepted request must succeed");
        let redecoded = UsersAdminRequest::decode(&buf[..len])
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(entries) = decode_user_list(bytes) {
        for entry in entries {
            let _ = entry;
        }
    }
    if let Ok(entries) = decode_group_list(bytes) {
        for entry in entries {
            let _ = entry;
        }
    }
}

/// Drive the System Information record family on `bytes` (one arm of
/// [`exercise`]): each accepted request/record round-trips through its
/// encoder.
fn exercise_sysinfo_records(bytes: &[u8]) {
    if let Ok(req) = ProcessListRequest::from_bytes(bytes) {
        let redecoded = ProcessListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(req) = MountListRequest::from_bytes(bytes) {
        let redecoded = MountListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(rec) = MountRecord::from_bytes(bytes) {
        let redecoded = MountRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
    if let Ok(rec) = ProcessRecord::from_bytes(bytes) {
        let redecoded = ProcessRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
    if let Ok(stats) = KernelMemoryStats::from_bytes(bytes) {
        let redecoded = KernelMemoryStats::from_bytes(&stats.to_le_bytes())
            .expect("round-trip of accepted stats must succeed");
        assert_eq!(stats, redecoded);
    }
    if let Ok(up) = Uptime::from_bytes(bytes) {
        let redecoded = Uptime::from_bytes(&up.to_le_bytes())
            .expect("round-trip of an accepted uptime must succeed");
        assert_eq!(up, redecoded);
    }
    if let Ok(id) = SystemIdentity::from_bytes(bytes) {
        let redecoded = SystemIdentity::from_bytes(&id.to_le_bytes())
            .expect("round-trip of an accepted identity must succeed");
        assert_eq!(id, redecoded);
    }
    if let Ok(req) = SeatListRequest::from_bytes(bytes) {
        let redecoded = SeatListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(rec) = SeatRecord::from_bytes(bytes) {
        let redecoded = SeatRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
    if let Ok(stats) = MemoryPressureStats::from_bytes(bytes) {
        let redecoded = MemoryPressureStats::from_bytes(&stats.to_le_bytes())
            .expect("round-trip of accepted pressure stats must succeed");
        assert_eq!(stats, redecoded);
    }
    if let Ok(req) = ReclaimListRequest::from_bytes(bytes) {
        let redecoded = ReclaimListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(rec) = ReclaimClassRecord::from_bytes(bytes) {
        let redecoded = ReclaimClassRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
    if let Ok(req) = CacheLedgerListRequest::from_bytes(bytes) {
        let redecoded = CacheLedgerListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(req) = CacheReportRequest::from_bytes(bytes) {
        let redecoded = CacheReportRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(rec) = CacheLedgerRecord::from_bytes(bytes) {
        let redecoded = CacheLedgerRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
        // A row that decoded is a row the whole system will fold and
        // render, so the label must already be renderable and the fold
        // must survive it — an accepted record can never be a shape a
        // later stage has to defend against a second time.
        assert!(rec.label().is_ascii() && !rec.label().is_empty());
        let totals = fold_cache_ledgers(&[rec, rec]);
        let total = totals
            .get(usize::from(rec.class))
            .expect("an accepted record's class indexes the fold");
        assert_eq!(
            total.payload_bytes,
            rec.payload_bytes.saturating_mul(2),
            "the fold must saturate rather than wrap"
        );
    }
    if let Ok(stats) = RamzipStats::from_bytes(bytes) {
        let redecoded = RamzipStats::from_bytes(&stats.to_le_bytes())
            .expect("round-trip of accepted ramzip stats must succeed");
        assert_eq!(stats, redecoded);
    }
    if let Ok(req) = CpuLoadRequest::from_bytes(bytes) {
        let redecoded = CpuLoadRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(rec) = CpuLoadRecord::from_bytes(bytes) {
        let redecoded = CpuLoadRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
}

/// The desktop frame-accounting decoders, split out so the sysinfo sweep above
/// stays inside one screen.
fn exercise_desktop_frame_records(bytes: &[u8]) {
    if let Ok(req) = DesktopFrameStatsRequest::from_bytes(bytes) {
        let redecoded = DesktopFrameStatsRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(totals) = DesktopFrameTotals::from_bytes(bytes) {
        let redecoded = DesktopFrameTotals::from_bytes(&totals.to_le_bytes())
            .expect("round-trip of accepted totals must succeed");
        assert_eq!(totals, redecoded);
        // Accepted totals are totals a reader will divide by: the bounds the
        // decoder enforced must still hold of the value it handed back, so a
        // consumer never has to defend against a shape twice.
        assert!(totals.frames > 0 || totals == DesktopFrameTotals::ZERO);
        assert!(totals.peak_damaged_px <= totals.damaged_px);
        assert!(totals.opaque_px <= totals.damaged_px);
    }
    if let Ok(rec) = DesktopFrameRecord::from_bytes(bytes) {
        let redecoded = DesktopFrameRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
        assert_ne!(rec.reporter_pid, 0, "a served row always names a publisher");
    }
}

/// Drive the service unit-metadata decoder on `bytes` (one arm of
/// [`exercise`]): the manager parses this record out of a service's signed
/// bundle, so it must refuse a corrupt one cleanly, never panic. The record
/// has a canonical encoding — the reserved field, the connect-capability
/// field, and the linger span are all forced to zero unless their flag says
/// otherwise — so an accepted record re-encodes to exactly the accepted
/// bytes and decodes back to an equal view.
fn exercise_service_manifest(bytes: &[u8]) {
    if let Ok(manifest) = ServiceManifest::from_bytes(bytes) {
        let requires: Vec<ReadyCondition> = manifest.requires().collect();
        let provides: Vec<ReadyCondition> = manifest.provides().collect();
        let dependencies: Vec<&str> = manifest.dependencies().collect();
        let limits: Vec<ServiceLimit> = manifest.limits().collect();
        let unit = ServiceUnit {
            account: manifest.account(),
            readiness: manifest.readiness(),
            activation: manifest.activation(),
            restart: manifest.restart(),
            stop_grace: manifest.stop_grace(),
            connect_capability: manifest.connect_capability(),
            requires: &requires,
            provides: &provides,
            dependencies: &dependencies,
            limits: &limits,
            watchdog: manifest.watchdog(),
        };
        let mut buf = vec![0u8; unit.encoded_len().expect("an accepted record has a length")];
        let len = unit
            .encode(&mut buf)
            .expect("round-trip encode of an accepted record must succeed");
        let redecoded = ServiceManifest::from_bytes(&buf[..len])
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(manifest, redecoded);
        // The encoding is canonical, so it reproduces the accepted bytes.
        assert_eq!(&buf[..len], bytes);
    }
}

/// Drive the service-manager control decoders on `bytes` (one arm of
/// [`exercise`]): an accepted control request round-trips through its
/// canonical encoder, and the reply decoder — untrusted manager output the
/// control tool parses — must refuse a corrupt frame cleanly, never panic.
fn exercise_service_control(bytes: &[u8]) {
    if let Ok(request) = ServiceControlRequest::decode(bytes) {
        let mut buf = [0u8; SERVICE_CONTROL_REQUEST_LEN];
        let len = request
            .encode(&mut buf)
            .expect("round-trip encode of an accepted control request must succeed");
        let redecoded = ServiceControlRequest::decode(&buf[..len])
            .expect("round-trip of an accepted control request must succeed");
        assert_eq!(request, redecoded);
        // The encoding is canonical, so it reproduces the accepted bytes.
        assert_eq!(&buf[..len], bytes);
    }
    let _ = decode_service_control_reply(bytes);
}

/// Drive the datagram-socket ABI decoders on `bytes` (one arm of
/// [`exercise`]): an accepted socket request or delivered datagram must
/// round-trip through its encoder, and the two reply decoders — untrusted
/// service output a client parses — must refuse a corrupt frame cleanly,
/// never panic.
fn exercise_net_socket(bytes: &[u8]) {
    if let Ok(request) = SocketRequest::from_bytes(bytes) {
        let mut buf = vec![0u8; SocketRequest::MAX_WIRE_LEN];
        let len = request
            .encode(&mut buf)
            .expect("round-trip encode of an accepted socket request must succeed");
        let redecoded = SocketRequest::from_bytes(&buf[..len])
            .expect("round-trip of an accepted socket request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(datagram) = SocketDatagram::parse(bytes) {
        let mut buf = vec![0u8; SocketDatagram::MAX_WIRE_LEN];
        let len = datagram
            .encode(&mut buf)
            .expect("round-trip encode of an accepted datagram must succeed");
        let reparsed = SocketDatagram::parse(&buf[..len])
            .expect("round-trip of an accepted datagram must succeed");
        assert_eq!(datagram, reparsed);
    }
    if let Ok(event) = SocketStreamEvent::parse(bytes) {
        let mut buf = vec![0u8; SocketStreamEvent::MAX_WIRE_LEN];
        let len = event
            .encode(&mut buf)
            .expect("round-trip encode of an accepted stream event must succeed");
        let reparsed = SocketStreamEvent::parse(&buf[..len])
            .expect("round-trip of an accepted stream event must succeed");
        assert_eq!(event, reparsed);
    }
    let _ = decode_socket_reply(bytes);
    let _ = decode_bind_reply(bytes);
    let _ = decode_send_reply(bytes);
}

/// Drive the cross-process NIC device-channel decoders on `bytes` (one arm
/// of [`exercise`]): an accepted control request or receive-notify must
/// round-trip through its encoder, and the two reply decoders — untrusted
/// driver output the stack parses — must refuse a corrupt frame cleanly,
/// never panic.
fn exercise_net_channel(bytes: &[u8]) {
    if let Ok(request) = NetChannelRequest::decode(bytes) {
        let mut buf = vec![0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = request
            .encode(&mut buf)
            .expect("round-trip encode of an accepted channel request must succeed");
        let redecoded = NetChannelRequest::decode(&buf[..len])
            .expect("round-trip of an accepted channel request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(notify) = NetChannelNotify::decode(bytes) {
        let redecoded = NetChannelNotify::decode(&notify.encode())
            .expect("round-trip of the notify frame must succeed");
        assert_eq!(notify, redecoded);
    }
    let _ = decode_facts_reply(bytes);
    let _ = decode_service_reply(bytes);
}

/// Drive the seat-manager protocol decoders on `bytes` (one arm of
/// [`exercise`]): an accepted seat-administration request must round-trip
/// through its encoder, and the shared status-reply decoder must refuse a
/// corrupt status word cleanly, never panic.
fn exercise_seatmgr(bytes: &[u8]) {
    if let Ok(request) = SeatAdminRequest::from_bytes(bytes) {
        let redecoded = SeatAdminRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    let _ = decode_status_reply(bytes);
}

/// Drive the display-service protocol decoders on `bytes` (one arm of
/// [`exercise`]): an accepted display request must round-trip through its
/// encoder, and the mode-reply decoder — untrusted service output a client
/// parses — must refuse a corrupt frame cleanly, never panic.
fn exercise_display_ipc(bytes: &[u8]) {
    if let Ok(request) = DisplayRequest::from_bytes(bytes) {
        let redecoded = DisplayRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    let _ = decode_mode_reply(bytes);
}

/// Drive the font-service protocol decoders on `bytes` (one arm of
/// [`exercise`]): an accepted font request must round-trip through its
/// encoder, and the glyph-coverage, metrics, and family-list reply decoders
/// — untrusted service output a text-drawing client parses — must refuse a
/// corrupt frame cleanly, never panic. An accepted glyph or family reply
/// additionally round-trips through its encoder, exercising the
/// variable-length coverage and record framing.
fn exercise_font_ipc(bytes: &[u8]) {
    if let Ok(request) = FontRequest::from_bytes(bytes) {
        let redecoded = FontRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(coverage) = decode_glyph_reply(bytes) {
        let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
        let len = encode_glyph_reply(&mut buf, &coverage)
            .expect("round-trip encode of an accepted glyph reply must succeed");
        let redecoded = decode_glyph_reply(&buf[..len])
            .expect("round-trip of an accepted glyph reply must succeed");
        assert_eq!(coverage, redecoded);
    }
    if let Ok(list) = decode_families_reply(bytes) {
        let mut buf = vec![0u8; FONT_MAX_FAMILIES_REPLY];
        let len = encode_families_reply(&mut buf, Ok(list.entries()))
            .expect("round-trip encode of an accepted family list must succeed");
        let redecoded = decode_families_reply(&buf[..len])
            .expect("round-trip of an accepted family list must succeed");
        assert_eq!(list, redecoded);
    }
    // A family key is built from bytes a client may take from a stored
    // preference, so its spelling check is attacker-reachable on its own.
    if bytes.len() >= 16 {
        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes[..16]);
        if let Ok(key) = FamilyKey::from_wire(key) {
            assert_eq!(FamilyKey::from_wire(key.to_wire()), Ok(key));
            assert!(!key.as_str().is_empty());
        }
    }
    let _ = decode_metrics_reply(bytes);
}

/// Drive the window-channel protocol decoders on `bytes` (one arm of
/// [`exercise`]): an accepted window request or event must round-trip
/// through its encoder, and the reply decoders — untrusted session output
/// an app parses — must refuse a corrupt frame cleanly, never panic.
fn exercise_window_ipc(bytes: &[u8]) {
    if let Ok(request) = WindowRequest::from_bytes(bytes) {
        // An accepted request is exactly as long as its own operation, so
        // re-encoding it must reproduce the very bytes that were accepted.
        let mut frame = [0u8; WindowRequest::MAX_WIRE_LEN];
        let len = request
            .encode(&mut frame)
            .expect("the max frame holds any request");
        assert_eq!(&frame[..len], bytes);
        let redecoded = WindowRequest::from_bytes(&frame[..len])
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(event) = WindowEvent::from_bytes(bytes) {
        let redecoded = WindowEvent::from_bytes(&event.to_le_bytes())
            .expect("round-trip of an accepted event must succeed");
        assert_eq!(event, redecoded);
    }
    let _ = decode_create_reply(bytes);
    let _ = decode_desktop_reply(bytes);
}

/// Drive the notification-channel decoder on `bytes` (one arm of
/// [`exercise`]): an accepted notify request must round-trip through its
/// encoder; a corrupt frame must refuse cleanly, never panic.
fn exercise_notify_ipc(bytes: &[u8]) {
    if let Ok(request) = NotifyRequest::from_bytes(bytes) {
        let redecoded = NotifyRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted notify request must succeed");
        assert_eq!(request, redecoded);
    }
}

/// Drive the pinboard-apply decoder on `bytes` (one arm of [`exercise`]):
/// an accepted apply request must round-trip through its encoder; a
/// corrupt frame must refuse cleanly, never panic.
fn exercise_pinboard_ipc(bytes: &[u8]) {
    if let Ok(request) = PinboardRequest::from_bytes(bytes) {
        let redecoded = PinboardRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted pinboard request must succeed");
        assert_eq!(request, redecoded);
    }
}

/// Drive both directions of the Switchboard channel on `bytes` (one arm of
/// [`exercise`]): an accepted request or command must round-trip through its
/// encoder; a corrupt frame must refuse cleanly, never panic.
fn exercise_switchboard_ipc(bytes: &[u8]) {
    if let Ok(request) = SwitchboardRequest::from_bytes(bytes) {
        let redecoded = SwitchboardRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted switchboard request must succeed");
        assert_eq!(request, redecoded);
    }
    // The reverse direction is attacker-reachable too: any process may send
    // a Switchboard instance's mailbox a frame, and the monitor decodes it
    // before it can check who sent it.
    if let Ok(command) = SwitchboardCommand::from_bytes(bytes) {
        let redecoded = SwitchboardCommand::from_bytes(&command.to_le_bytes())
            .expect("round-trip of an accepted switchboard command must succeed");
        assert_eq!(command, redecoded);
    }
    // The publish reply carries the session identity the monitor authenticates
    // every later command against, so a malformed reply must refuse rather
    // than yield an identity.
    let _ = decode_publish_reply(bytes);
}

/// Drive the app-data channel on `bytes` (one arm of [`exercise`]).
///
/// Every application on the machine may post to this endpoint, so the request
/// decoder is reachable by any process; and a caller decodes the daemon's own
/// replies, so both directions are attacker-reachable from one side or the
/// other. An accepted frame must round-trip; a corrupt one must refuse
/// cleanly, never panic.
fn exercise_appdata_ipc(bytes: &[u8]) {
    if let Ok(request) = AppDataRequest::decode(bytes) {
        let mut buf = [0u8; APPDATA_MAX_REQUEST];
        let len = request
            .encode(&mut buf)
            .expect("an accepted request must re-encode");
        let redecoded = AppDataRequest::decode(&buf[..len])
            .expect("round-trip of an accepted app-data request must succeed");
        assert_eq!(request, redecoded);
    }
    // The document reply is the other attacker-reachable direction: a client
    // decodes whatever the endpoint answered before it can trust any of it.
    // A whole document must round-trip; a capacity refusal must stay one.
    match decode_document_reply(bytes) {
        Ok(ConfigDocument::Whole(document)) => {
            // A buffer of exactly the document's own length is the smallest
            // capacity that must still deliver it whole.
            let capacity = u32::try_from(document.len()).expect("bounded by the document max");
            // Heap, not stack: a whole-document reply is 64 KiB wide.
            let mut buf = vec![0u8; APPDATA_MAX_REPLY];
            let len = encode_document_reply(document, capacity, &mut buf)
                .expect("an accepted document must re-encode");
            assert_eq!(
                decode_document_reply(&buf[..len]),
                Ok(ConfigDocument::Whole(document)),
                "round-trip of an accepted document reply must succeed"
            );
            // …and one byte short of it must refuse with the length to retry
            // at, never a truncated body.
            if !document.is_empty() {
                let len = encode_document_reply(document, capacity - 1, &mut buf)
                    .expect("a capacity refusal must encode");
                assert_eq!(
                    decode_document_reply(&buf[..len]),
                    Ok(ConfigDocument::NeedsCapacity(document.len()))
                );
            }
        }
        Ok(ConfigDocument::NeedsCapacity(needed)) => {
            assert!(needed > 0 && needed <= APPDATA_DOCUMENT_MAX);
        }
        Err(_) => {}
    }
    // The blob replies are the same two directions again: a caller decodes a
    // grant handle, a listing, and a quota before it can act on any of them,
    // and every one of the three is a frame a hostile or damaged service could
    // have produced.
    if let Ok(handle) = decode_grant_reply(bytes) {
        assert_ne!(handle, 0, "a decoded grant handle is never the invalid one");
        let mut buf = [0u8; APPDATA_GRANT_REPLY_LEN];
        let len = encode_grant_reply(handle, &mut buf).expect("an accepted handle must re-encode");
        assert_eq!(decode_grant_reply(&buf[..len]), Ok(handle));
    }
    match decode_blob_list_reply(bytes) {
        Ok(BlobListing::Whole(listing)) => {
            assert!(listing.len().is_multiple_of(APPDATA_BLOB_ENTRY_LEN));
            // Walking a whole listing must terminate and yield no more entries
            // than the body can hold, whatever the entries themselves say.
            assert!(
                decode_blob_list_reply(bytes)
                    .expect("just decoded")
                    .entries()
                    .count()
                    <= listing.len() / APPDATA_BLOB_ENTRY_LEN
            );
            let capacity = u32::try_from(listing.len()).expect("bounded by the listing max");
            let mut buf = vec![0u8; APPDATA_MAX_REPLY];
            let len = encode_blob_list_reply(listing, capacity, &mut buf)
                .expect("an accepted listing must re-encode");
            assert_eq!(
                decode_blob_list_reply(&buf[..len]),
                Ok(BlobListing::Whole(listing))
            );
            if !listing.is_empty() {
                let len = encode_blob_list_reply(listing, capacity - 1, &mut buf)
                    .expect("a capacity refusal must encode");
                assert_eq!(
                    decode_blob_list_reply(&buf[..len]),
                    Ok(BlobListing::NeedsCapacity(listing.len()))
                );
            }
        }
        Ok(BlobListing::NeedsCapacity(needed)) => {
            assert!(needed > 0 && needed <= APPDATA_BLOB_LIST_MAX);
            assert!(needed.is_multiple_of(APPDATA_BLOB_ENTRY_LEN));
        }
        Err(_) => {}
    }
    if let Ok(quota) = decode_quota_reply(bytes) {
        let mut buf = [0u8; APPDATA_QUOTA_REPLY_LEN];
        let len = encode_quota_reply(&quota, &mut buf).expect("an accepted quota must re-encode");
        assert_eq!(decode_quota_reply(&buf[..len]), Ok(quota));
    }
    // A temporary-file reply carries a name the caller hands straight back to
    // a release, so an accepted one must already be inside the store-name
    // grammar — never a fragment a caller could compose a path from.
    if let Ok((handle, name)) = decode_temp_reply(bytes) {
        assert_ne!(handle, 0, "a decoded grant handle is never the invalid one");
        assert!(tairix_abi::appdata_ipc::validate_bulk_name(name).is_ok());
        let mut buf = [0u8; APPDATA_TEMP_REPLY_LEN];
        let len =
            encode_temp_reply(handle, name, &mut buf).expect("an accepted reply must re-encode");
        assert_eq!(decode_temp_reply(&buf[..len]), Ok((handle, name)));
    }
}

/// Drive every ABI decoder on `bytes`.
///
/// Returns silently. The contract is "must not panic for any input"; a
/// successful decode is additionally required to round-trip through its
/// matching encoder.
fn exercise(bytes: &[u8]) {
    if let Ok(header) = IpcMessageHeader::from_bytes(bytes) {
        let encoded = header.to_le_bytes();
        let redecoded = IpcMessageHeader::from_bytes(&encoded)
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(header) = ManifestHeader::from_bytes(bytes) {
        let encoded = header.to_le_bytes();
        let redecoded = ManifestHeader::from_bytes(&encoded)
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(header) = AppInfoHeader::from_bytes(bytes) {
        let redecoded = AppInfoHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
        // The publisher classification is total over every decodable
        // manifest, and both derived messages are fixed-size and labelled,
        // so a hostile header can neither panic nor produce an unlabelled
        // message some other signature could be replayed into.
        let _ = header.publisher_binding();
        assert!(header
            .publisher_cert_message()
            .starts_with(PUBLISHER_CERT_CONTEXT));
        assert!(header
            .publisher_id_preimage()
            .starts_with(PUBLISHER_ID_CONTEXT));
    }
    if let Ok(origin) = Origin::from_bytes(bytes) {
        let redecoded = Origin::from_bytes(&origin.to_le_bytes())
            .expect("round-trip of an accepted origin must succeed");
        assert_eq!(origin, redecoded);
        // An accepted origin's app identity is whole or absent, never half:
        // the identifier is inside the grammar that keeps it a legal store
        // name, and it never appears without a publisher to own it.
        if let Some(app) = origin.app() {
            assert!(tairix_abi::validate_bundle_id(app.bundle_id()).is_ok());
            assert!(!app.publisher().is_none());
        }
    }
    if let Ok(header) = SysinfoRequestHeader::from_bytes(bytes) {
        let redecoded = SysinfoRequestHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    exercise_service_manifest(bytes);
    exercise_service_control(bytes);
    exercise_users_admin(bytes);
    exercise_sysinfo_records(bytes);
    exercise_desktop_frame_records(bytes);
    exercise_seatmgr(bytes);
    exercise_display_ipc(bytes);
    exercise_font_ipc(bytes);
    exercise_window_ipc(bytes);
    exercise_notify_ipc(bytes);
    exercise_pinboard_ipc(bytes);
    exercise_appdata_ipc(bytes);
    exercise_switchboard_ipc(bytes);
    exercise_net_socket(bytes);
    exercise_net_channel(bytes);
    exercise_elevate(bytes);
    exercise_session_ipc(bytes);
    if let Ok(time) = Time64::from_bytes(bytes) {
        let redecoded = Time64::from_bytes(&time.to_le_bytes())
            .expect("round-trip of an accepted instant must succeed");
        assert_eq!(time, redecoded);
    }
    if let Ok(duration) = Duration64::from_bytes(bytes) {
        let redecoded = Duration64::from_bytes(&duration.to_le_bytes())
            .expect("round-trip of an accepted duration must succeed");
        assert_eq!(duration, redecoded);
    }
    if let Ok(event) = PointerInput::from_bytes(bytes) {
        let redecoded = PointerInput::from_bytes(&event.to_le_bytes())
            .expect("round-trip of an accepted pointer event must succeed");
        assert_eq!(event, redecoded);
    }
    if let Ok(event) = KeyInput::from_bytes(bytes) {
        let redecoded = KeyInput::from_bytes(&event.to_le_bytes())
            .expect("round-trip of an accepted key event must succeed");
        assert_eq!(event, redecoded);
    }
    exercise_rlimit(bytes);
    if let Ok(name) = PortName::from_bytes(bytes) {
        let redecoded = PortName::from_bytes(&name.to_le_bytes())
            .expect("round-trip of an accepted port name must succeed");
        assert_eq!(name, redecoded);
    }
    if let Ok(lib) = NeededLibrary::decode(bytes) {
        let redecoded = NeededLibrary::decode(&lib.to_le_bytes())
            .expect("round-trip of an accepted needed-library record must succeed");
        assert_eq!(lib, redecoded);
    }
    // The whole-image loader has no single round-trip encoder (the builder is
    // test-only), so the contract here is the "must not panic for any
    // input"; an accepted image must additionally re-parse deterministically
    // and yield resolvable needed-library references.
    if let Ok(image) = LoadImage::parse(bytes, &FUZZ_CFI_TAG) {
        let reparsed = LoadImage::parse(bytes, &FUZZ_CFI_TAG)
            .expect("re-parse of an accepted load image must succeed");
        assert_eq!(image, reparsed);
        for name in image.needed_libraries() {
            assert!(!name.is_empty());
        }
    }
    exercise_process(bytes);
    exercise_usb_urb(bytes);
    exercise_blkio(bytes);
    exercise_fs(bytes);
    exercise_introspect(bytes);
}

/// Drive the System Information introspection decoders on `bytes`.
///
/// Split out of [`exercise`] so each helper stays a single, readable unit;
/// the contract is identical (must not panic for any input; an accepted
/// decode round-trips through its matching encoder). These are the decoders a
/// `sysinfo` client feeds with bytes a possibly-hostile server produced: the
/// framed reply a client parses off the synchronous call transport, and the
/// closed introspection-domain selector.
fn exercise_introspect(bytes: &[u8]) {
    // The reply frame is untrusted server output. On success the decoder
    // returns a borrowed payload slice; re-framing that payload must yield
    // exactly the original bytes (the status word was zero). A non-zero status
    // is either a defined server `Errno` or a fail-closed `OutOfRange` — never
    // a panic.
    if let Ok(payload) = decode_reply(bytes) {
        let mut buf = vec![0u8; SYSINFO_REPLY_STATUS_LEN + payload.len()];
        let written =
            encode_reply_ok(payload, &mut buf).expect("an accepted reply payload must re-frame");
        assert_eq!(&buf[..written], bytes);
    }
    if bytes.len() >= 4 {
        let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if let Ok(domain) = IntrospectDomain::from_u32(raw) {
            // An accepted discriminant round-trips through its raw encoding.
            assert_eq!(IntrospectDomain::from_u32(domain.as_u32()), Ok(domain));
            assert_eq!(domain.as_u32(), raw);
        }
    }
}

/// Drive the `fs` wire decoders on `bytes`.
///
/// Split out of [`exercise`] so each helper stays a single, readable unit;
/// the contract is identical (must not panic for any input; an accepted
/// decode round-trips through its matching encoder). These are the
/// filesystem-ABI decoders a userland file client and the `fs_*` reply path
/// feed with buffers a possibly-hostile peer produced.
fn exercise_fs(bytes: &[u8]) {
    if let Ok(stat) = FileStat::decode(bytes) {
        let mut buf = [0u8; FileStat::WIRE_LEN];
        let written = stat
            .encode(&mut buf)
            .expect("an accepted FileStat must re-encode");
        assert_eq!(written, FileStat::WIRE_LEN);
        let redecoded =
            FileStat::decode(&buf).expect("round-trip of an accepted FileStat must succeed");
        assert_eq!(stat, redecoded);
    }
    if let Ok((entry, consumed)) = DirEntry::decode(bytes) {
        // The reported consumed length is exactly the record's encoded size,
        // so a reader walking a packed stream advances correctly.
        assert_eq!(consumed, entry.encoded_len());
        let mut buf = [0u8; DirEntry::HEADER_LEN + FS_NAME_MAX];
        let written = entry
            .encode_into(&mut buf)
            .expect("an accepted DirEntry must re-encode");
        assert_eq!(written, consumed);
        let (redecoded, reconsumed) =
            DirEntry::decode(&buf).expect("round-trip of an accepted DirEntry must succeed");
        assert_eq!(entry, redecoded);
        assert_eq!(reconsumed, consumed);
    }
    // The whole-stream walker must terminate on any input, make the forward
    // progress its contract states, and fuse after the first refusal — a
    // hostile stream ends the walk with one clean error, never a panic or a
    // stall.
    let mut walked = 0usize;
    let mut refused = false;
    for item in DirEntries::new(bytes) {
        assert!(!refused, "the walker must fuse after its first refusal");
        match item {
            Ok(entry) => walked += entry.encoded_len(),
            Err(_) => refused = true,
        }
        assert!(
            walked <= bytes.len(),
            "the walker must never claim more bytes than the stream holds"
        );
    }
    // `FileKind`/`OpenFlags` decode from a scalar rather than a slice; derive
    // the scalar from the fuzz bytes so the boundary between accepted and
    // rejected values is still walked.
    if let Some(&raw) = bytes.first() {
        if let Ok(kind) = FileKind::from_u8(raw) {
            assert_eq!(FileKind::from_u8(kind.as_u8()), Ok(kind));
        }
    }
    if bytes.len() >= 4 {
        let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if let Ok(flags) = OpenFlags::from_bits(raw) {
            assert_eq!(OpenFlags::from_bits(flags.bits()), Ok(flags));
        }
    }
}

/// Drive the URB transport decoders on `bytes`.
///
/// Split out of [`exercise`] so each helper stays a single, readable unit;
/// the contract is identical (must not panic; an accepted decode round-trips
/// through its encoder). The completion frame has no struct, so its decoder
/// is exercised directly for the "must not panic" half of the contract.
fn exercise_usb_urb(bytes: &[u8]) {
    use tairix_abi::usb_urb::{UrbRequest, URB_REQUEST_LEN};
    if let Ok(req) = UrbRequest::decode(bytes) {
        let mut buf = [0u8; URB_REQUEST_LEN];
        req.encode(&mut buf)
            .expect("an accepted URB request must re-encode");
        let redecoded =
            UrbRequest::decode(&buf).expect("round-trip of an accepted URB request must succeed");
        assert_eq!(req, redecoded);
    }
    // The completion decoder accepts any byte slice and either reports the
    // transferred count or a fail-closed errno; the contract is that it never
    // panics for an arbitrary input.
    let _ = tairix_abi::usb_urb::decode_completion(bytes);
}

/// Drive the block-service transport decoders on `bytes`.
///
/// Split out of [`exercise`] so each helper stays a single, readable unit;
/// the contract is identical (must not panic; an accepted decode round-trips
/// through its encoder). The completion decoder is exercised directly for
/// the "must not panic" half of the contract, exactly as the URB one.
fn exercise_blkio(bytes: &[u8]) {
    use tairix_abi::blkio::{BlkRequest, BLK_REQUEST_LEN};
    if let Ok(req) = BlkRequest::decode(bytes) {
        let mut buf = [0u8; BLK_REQUEST_LEN];
        req.encode(&mut buf)
            .expect("an accepted block-service request must re-encode");
        let redecoded = BlkRequest::decode(&buf)
            .expect("round-trip of an accepted block-service request must succeed");
        assert_eq!(req, redecoded);
    }
    let _ = tairix_abi::blkio::decode_completion(bytes);
    // The full health-axis decoder must also never panic and must fail
    // closed to `Fatal`/`DeviceFault` on a truncated or unknown-status frame
    // rather than reading garbage as a valid completion (`plans/FIX-IO.md`
    // IO1). The returned outcome is always self-consistent: a data-valid
    // status carries the geometry, every other status carries an error.
    let outcome = tairix_abi::blkio::decode_outcome(bytes);
    assert_eq!(outcome.status.data_valid(), outcome.data().is_ok());
}

/// Drive the resource-limit decoder on `bytes`.
///
/// Split out of [`exercise`] to keep that function within the line budget;
/// the contract is identical (must not panic; an accepted decode round-trips
/// through its encoder and is well-formed).
fn exercise_rlimit(bytes: &[u8]) {
    if let Ok(limit) = ResourceLimit::decode(bytes) {
        let redecoded = ResourceLimit::decode(&limit.encode())
            .expect("round-trip of an accepted resource limit must succeed");
        assert_eq!(limit, redecoded);
        // An accepted limit is always well-formed (`soft <= hard`).
        assert!(limit.is_well_formed());
    }
    if let Ok(rec) = ResourceLimitRecord::from_bytes(bytes) {
        let redecoded = ResourceLimitRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted resource-limit record must succeed");
        assert_eq!(rec, redecoded);
        // The embedded limit is always well-formed and the reserved word zero.
        assert!(rec.limit.is_well_formed());
        assert_eq!(rec.reserved, 0);
    }
}

/// Drive the `process` startup-vector decoders on `bytes`.
///
/// Split out of [`exercise`] so each helper stays a single, readable unit;
/// the contract is identical (must not panic; an accepted decode round-trips
/// or re-parses deterministically).
fn exercise_process(bytes: &[u8]) {
    if let Ok(header) = ProcessStartHeader::from_bytes(bytes) {
        let redecoded = ProcessStartHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted start header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(slot) = StringSlot::from_bytes(bytes) {
        let redecoded = StringSlot::from_bytes(&slot.to_le_bytes())
            .expect("round-trip of an accepted string slot must succeed");
        assert_eq!(slot, redecoded);
    }
    if let Ok(view) = ProcessStart::parse(bytes) {
        // The view borrows `bytes`; re-parsing the same bytes must be
        // deterministic, and every accepted string must resolve.
        let reparsed = ProcessStart::parse(bytes)
            .expect("re-parse of an accepted startup vector must succeed");
        assert_eq!(view, reparsed);
        for i in 0..view.arg_count() {
            assert!(view.arg(i).is_some());
        }
        for i in 0..view.env_count() {
            assert!(view.env(i).is_some());
        }
    }
    exercise_process_builder(bytes);
}

/// Drive the production startup-vector *builder* on `bytes`.
///
/// The fuzz bytes are split on `0xFF` into argument/environment strings and
/// fed to [`tairix_abi::process::write_into`]; an accepted build must parse
/// back to exactly those strings, and a rejected build (e.g. an embedded NUL)
/// must fail closed rather than panic.
fn exercise_process_builder(bytes: &[u8]) {
    let mut parts: Vec<&[u8]> = bytes.split(|&b| b == 0xFF).collect();
    // Keep the builder cheap and comfortably within the abi-v1 limits.
    parts.truncate(8);
    let split = parts.len() / 2;
    let (args, env) = parts.split_at(split);

    let mut seed = [0u8; 8];
    let take = core::cmp::min(8, bytes.len());
    seed[..take].copy_from_slice(&bytes[..take]);
    let canary = u64::from_le_bytes(seed);
    // A distinct value in the cpu-features field, to prove it round-trips
    // independently of the canary.
    let cpu_features = canary.rotate_left(17) ^ 0x0F0F_0F0F_0F0F_0F0F;

    let Ok(len) = tairix_abi::process::encoded_len(args, env) else {
        return;
    };
    let mut buf = vec![0u8; len];
    let Ok(written) = tairix_abi::process::write_into(&mut buf, args, env, canary, cpu_features)
    else {
        // A rejected build (an embedded NUL, say) is a fail-closed outcome.
        return;
    };
    assert_eq!(written, len);
    let view = ProcessStart::parse(&buf).expect("a freshly built block must parse");
    assert_eq!(view.arg_count() as usize, args.len());
    assert_eq!(view.env_count() as usize, env.len());
    assert_eq!(view.canary(), canary);
    assert_eq!(view.cpu_features().bits(), cpu_features);
    let mut idx: u32 = 0;
    for a in args {
        assert_eq!(view.arg(idx), Some(*a));
        idx += 1;
    }
    idx = 0;
    for e in env {
        assert_eq!(view.env(idx), Some(*e));
        idx += 1;
    }
}

#[test]
fn random_short_inputs_never_panic() {
    let mut rng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "random_short_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 256];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            // Random size in [0, buf.len()].
            // Mask to a width that fits any usize then range-reduce. The
            // bitmask makes the cast lossless without depending on
            // target-pointer width.
            let size = ((rng.next_u64() & 0xFFFF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    // Start from a well-formed IPC header, then bit-flip individual bytes
    // to walk the boundary between accepted and rejected.
    let mut base = IpcMessageHeader {
        magic: tairix_abi::IPC_MESSAGE_HEADER_MAGIC,
        version: 1,
        flags: 0,
        endpoint: 0xDEAD_BEEF_CAFE_F00D,
        sender: 0,
        payload_len: 16,
        reserved: 0,
    }
    .to_le_bytes();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }
}

#[test]
fn structured_cache_ledger_inputs_with_corrupted_fields_never_panic() {
    // A cache-ledger row is 128 bytes with a validated label, so a random
    // short input will essentially never produce one; only a bit-flip sweep
    // from a well-formed row actually walks its accept/reject boundary. It
    // is worth walking: on a report submission every one of these fields
    // arrives from another process.
    let mut row = CacheLedgerRecord::new(
        b"session.desktop-artwork",
        tairix_abi::sysinfo::CacheOwnerKind::DesktopSession,
        1,
        0,
    )
    .expect("a well-formed cache-ledger row encodes");
    row.origin = tairix_abi::sysinfo::CacheLedgerOrigin::SelfReported;
    row.reporter_pid = 41;
    row.payload_bytes = u64::MAX - 1;
    row.metadata_bytes = 4096;
    row.entries = 12;
    row.hits = 900;
    row.misses = 100;

    let mut base = row.to_le_bytes();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }
}

#[test]
fn structured_fs_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the filesystem decoders from
    // well-formed images: a `FileStat` and a packed `DirEntry`. Bit-flipping
    // each byte drives the kind/length/reserved-field checks without ever
    // panicking.
    let mut stat = [0u8; FileStat::WIRE_LEN];
    FileStat {
        kind: FileKind::Regular,
        nlink: 2,
        size: 0xDEAD_BEEF,
        allocated: 0xF00D_0000,
        mode: 0o644,
        uid: 1000,
        gid: 1000,
        id: tairix_abi::FileId::NONE,
        // Non-trivial stamps so the bit-flip sweep also walks the
        // timestamp-decode branch of `FileStat::decode`.
        times: tairix_abi::NodeTimes {
            created: Time64::from_secs(-2_000_000_000),
            modified: Time64::new(4_000_000_000, 999_999_999).expect("canonical"),
            accessed: Time64::UNIX_EPOCH,
            changed: Time64::from_secs(1_700_000_000),
        },
    }
    .encode(&mut stat)
    .expect("a well-formed FileStat encodes");

    let mut dirent = [0u8; DirEntry::HEADER_LEN + 5];
    DirEntry {
        kind: FileKind::Directory,
        size: 0,
        allocated: 4096,
        modified: Time64::new(1_234_567_890, 987_654_321).expect("canonical"),
        // A non-trivial identity and count so the sweep also flips every
        // bit of the two fields the listing gained.
        id: tairix_abi::FileId {
            volume: [0x5a; 16],
            node: 0x0102_0304_0506_0708,
        },
        nlink: 3,
        name: b"inbox",
    }
    .encode_into(&mut dirent)
    .expect("a well-formed DirEntry encodes");

    for base in [stat.as_mut_slice(), dirent.as_mut_slice()] {
        for byte in 0..base.len() {
            for bit in 0..8u32 {
                base[byte] ^= 1 << bit;
                exercise(base);
                base[byte] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn structured_reply_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the untrusted `sysinfo` reply
    // decoder from a well-formed success frame (a zero status word followed by
    // a short payload). Bit-flipping the status word drives the success /
    // server-errno / fail-closed `OutOfRange` branches without ever panicking.
    let payload = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0u8; SYSINFO_REPLY_STATUS_LEN + payload.len()];
    let written = encode_reply_ok(&payload, &mut frame).expect("a well-formed reply frame encodes");
    assert_eq!(written, frame.len());

    for byte in 0..frame.len() {
        for bit in 0..8u32 {
            frame[byte] ^= 1 << bit;
            exercise(&frame);
            frame[byte] ^= 1 << bit;
        }
    }
}

/// Every window-channel operation, seeded and bit-flipped at its own frame
/// length as well as one byte either side of it.
///
/// A request is framed to its operation's own length and decoded only at
/// exactly that length, so a random-length input is refused on length before
/// it reaches any operand. Structured seeds are therefore what actually
/// exercises the operand decoders, and one per operation is what stops an
/// operation from having no coverage at all.
#[test]
fn structured_window_requests_with_corrupted_fields_never_panic() {
    let seeds = [
        WindowRequest::Create {
            shm_handle: 7,
            event_endpoint: 0x900d,
            frame_count: 2,
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
            title: WindowTitle::new("Documents").expect("a valid title"),
            sizing: WindowSizing::Resizable {
                min_width_px: 320,
                min_height_px: 240,
            },
        },
        WindowRequest::CreatePopup {
            parent_window_id: 3,
            shm_handle: 7,
            event_endpoint: 0x900d,
            frame_count: 1,
            width_px: 120,
            height_px: 80,
            stride_bytes: 480,
            format: DisplayFormat::Bgra8888,
            offset_x: -12,
            offset_y: 24,
        },
        WindowRequest::Present {
            window_id: 3,
            frame_index: 1,
            damage: DamageRect {
                x: 4,
                y: 8,
                width_px: 16,
                height_px: 32,
            },
        },
        WindowRequest::Close { window_id: 3 },
        WindowRequest::PickFile { window_id: 3 },
        WindowRequest::Resize {
            window_id: 3,
            shm_handle: 11,
            frame_count: 2,
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        },
        WindowRequest::SetTitle {
            window_id: 3,
            title: WindowTitle::new("Inbox").expect("a valid title"),
        },
        WindowRequest::SetBackdropBlur {
            window_id: 5,
            radius_px: 8,
        },
        WindowRequest::QueryDesktop,
    ];
    let mut base = [0u8; WindowRequest::MAX_WIRE_LEN + 1];
    for seed in seeds {
        let len = seed
            .encode(&mut base)
            .expect("the max frame holds any request");
        for byte in 0..len {
            for bit in 0..8u32 {
                base[byte] ^= 1 << bit;
                exercise(&base[..len]);
                exercise(&base[..len - 1]);
                exercise(&base[..=len]);
                base[byte] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn structured_icon_bar_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the icon-bar declaration and
    // its two outcome events from well-formed frames. A bit-flip lands as
    // readily on a row's kind, flag byte, parent index, label length, or
    // item id as on the header, and every one of them must fail closed.
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Submenu {
        label: AppMenuLabel::new("Display").expect("a valid label"),
        enabled: true,
    })
    .expect("room for a submenu");
    menu.push_under(
        AppMenuRow::Item {
            id: AppMenuItemId::new(1).expect("a valid id"),
            label: AppMenuLabel::new("Full screen").expect("a valid label"),
            enabled: true,
            mark: AppMenuMark::Check,
        },
        0,
    )
    .expect("room inside it");
    menu.push(AppMenuRow::Separator).expect("a separator");
    menu.push(AppMenuRow::About).expect("an About row");
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(2).expect("a valid id"),
        label: AppMenuLabel::new("Quit").expect("a valid label"),
        enabled: true,
        mark: AppMenuMark::None,
    })
    .expect("room for Quit");
    let declare = WindowRequest::SetAppBar(AppBar {
        event_endpoint: 0xE117_0000_0000_0009,
        default_action: true,
        menu,
    });
    // The seed is the frame a client actually sends — exactly the
    // declaration's own length. Feeding a padded buffer instead would be
    // refused on length alone and would never reach the row decoder these
    // flips exist to exercise.
    let mut base = [0u8; WindowRequest::MAX_WIRE_LEN + 1];
    let len = declare
        .encode(&mut base)
        .expect("the max frame holds any request");
    for byte in 0..len {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base[..len]);
            // A truncated and an over-long spelling of the same flipped
            // frame must both fail closed rather than be read short.
            exercise(&base[..len - 1]);
            exercise(&base[..=len]);
            base[byte] ^= 1 << bit;
        }
    }
    let events = [
        WindowEvent::AppBarDefault.to_le_bytes(),
        WindowEvent::AppBarMenu {
            item: AppMenuItemId::new(7).expect("a valid id"),
        }
        .to_le_bytes(),
    ];
    for mut base in events {
        for byte in 0..base.len() {
            for bit in 0..8u32 {
                base[byte] ^= 1 << bit;
                exercise(&base);
                base[byte] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn structured_notify_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the notification requests
    // from well-formed frames: a bit-flip of any byte — magic, version,
    // op, key, severity, a length prefix, or a title/body byte — must
    // fail closed, never panic.
    let raise = NotifyRequest::Raise {
        key: 0x1234_5678,
        severity: NotifySeverity::Warning,
        title: NotifyTitle::new("Battery low").expect("a valid title"),
        body: NotifyBody::new("12% remaining.").expect("a valid body"),
    }
    .to_le_bytes();
    let clear = NotifyRequest::Clear { key: 9 }.to_le_bytes();
    for mut base in [raise, clear] {
        for byte in 0..base.len() {
            for bit in 0..8u32 {
                base[byte] ^= 1 << bit;
                exercise(&base);
                base[byte] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn structured_pinboard_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the pinboard apply request
    // from a well-formed frame: a bit-flip of any byte — magic, version,
    // op, the reserved pair, the length prefix, or a document byte —
    // must fail closed, never panic.
    let mut base = PinboardRequest::Apply {
        document: PinboardDocument::new(
            "wallpaper none\nfit fill\nbackdrop theme\nicons leading\nsort name\n",
        )
        .expect("a valid document"),
    }
    .to_le_bytes();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }
}

#[test]
fn structured_switchboard_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the tray-summary publish
    // request from a well-formed frame carrying every optional block: a
    // bit-flip of any byte — magic, version, op, a count, the pressure
    // kind or level, a permille, a length prefix, or a name byte — must
    // fail closed, never panic.
    let mut base = SwitchboardRequest::PublishSummary {
        summary: TraySummary {
            jobs: 4,
            recovery: 1,
            cpu_busy_permille: TrayPermille::new(640).expect("a valid fraction"),
            pressure: Some(TrayPressure {
                kind: TrayPressureKind::Memory,
                level: TrayPermille::new(870).expect("a valid fraction"),
                count: TrayPressureCount::new(2).expect("a valid count"),
            }),
            top_task: Some(TrayTask {
                name: TrayTaskName::new("compositor").expect("a valid name"),
                cpu_permille: TrayPermille::new(250).expect("a valid fraction"),
            }),
            power_capable: true,
        },
    }
    .to_le_bytes();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }

    // The owner-directed operations reach a stranger's window, so walk their
    // boundary too: a flipped op, a flipped owner id, or a dirtied reserved
    // byte must refuse rather than resolve some other owner.
    for mut base in [
        SwitchboardRequest::ActivateOwner { owner: 0x0102_0304 }.to_le_bytes(),
        SwitchboardRequest::RestartOwner { owner: 7 }.to_le_bytes(),
    ] {
        for byte in 0..base.len() {
            for bit in 0..8u32 {
                base[byte] ^= 1 << bit;
                exercise(&base);
                base[byte] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn structured_switchboard_commands_with_corrupted_fields_never_panic() {
    // The session -> monitor direction is decoded from an unreserved
    // mailbox any process can send to, so walk the accepted/rejected
    // boundary of every command: a flipped section, count, total, owner
    // slot, frame count, or power-action discriminant must fail closed,
    // never panic.
    let open = SwitchboardCommand::OpenPanel {
        section: CommandSection::Recovery,
    }
    .to_le_bytes();
    let report = SwitchboardCommand::SeatReport {
        report: SeatReport::new(5, &[3, 9, 0x0102_0304]).expect("a truthful report"),
    }
    .to_le_bytes();
    let frame = SwitchboardCommand::FrameReport {
        report: FrameReport {
            screen_px: 1920 * 1080,
            damaged_px: 3_200,
            blended_px: 6_400,
            opaque_px: 1_100,
            dirty_rects: 2,
            present_calls: 2,
            chrome_hits: 1,
            chrome_misses: 0,
        },
    }
    .to_le_bytes();
    let power = SwitchboardCommand::Power {
        action: PowerAction::Restart,
    }
    .to_le_bytes();
    for mut base in [open, report, frame, power] {
        for byte in 0..base.len() {
            for bit in 0..8u32 {
                base[byte] ^= 1 << bit;
                exercise(&base);
                base[byte] ^= 1 << bit;
            }
        }
    }
}
