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

use tairix_abi::display_ipc::{decode_mode_reply, DisplayRequest};
use tairix_abi::driver::net_channel::{
    decode_facts_reply, decode_service_reply, NetChannelNotify, NetChannelRequest,
};
use tairix_abi::elevate::{ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN};
use tairix_abi::fs::{DirEntries, DirEntry, FileKind, FileStat, OpenFlags, FS_NAME_MAX};
use tairix_abi::input::{KeyInput, PointerInput};
use tairix_abi::net::{decode_bind_reply, decode_socket_reply, SocketDatagram, SocketRequest};
use tairix_abi::process::{ProcessStart, ProcessStartHeader, StringSlot};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::rlimit::ResourceLimit;
use tairix_abi::seat::SeatAdminRequest;
use tairix_abi::sysinfo::{
    decode_reply, encode_reply_ok, CpuLoadRecord, CpuLoadRequest, IntrospectDomain,
    KernelMemoryStats, MemoryPressureStats, MountListRequest, MountRecord, ProcessListRequest,
    ProcessRecord, RamzipStats, ReclaimClassRecord, ReclaimListRequest, ResourceLimitRecord,
    SeatListRequest, SeatRecord, SysinfoRequestHeader, SystemIdentity, Uptime,
    SYSINFO_REPLY_STATUS_LEN,
};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::users_admin::{
    decode_group_list, decode_user_list, UsersAdminRequest, USERS_ADMIN_MAX_REQUEST,
};
use tairix_abi::window_ipc::{decode_create_reply, WindowEvent, WindowRequest};
use tairix_abi::{
    AppInfoHeader, IpcMessageHeader, LoadImage, ManifestHeader, NeededLibrary, PortName,
    SYSCALL_TABLE_HASH_LEN,
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
    let _ = decode_socket_reply(bytes);
    let _ = decode_bind_reply(bytes);
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
        let redecoded = NetChannelNotify::decode(&NetChannelNotify::encode())
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

/// Drive the window-channel protocol decoders on `bytes` (one arm of
/// [`exercise`]): an accepted window request or event must round-trip
/// through its encoder, and the create-reply decoder — untrusted session
/// output an app parses — must refuse a corrupt frame cleanly, never
/// panic.
fn exercise_window_ipc(bytes: &[u8]) {
    if let Ok(request) = WindowRequest::from_bytes(bytes) {
        let redecoded = WindowRequest::from_bytes(&request.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(request, redecoded);
    }
    if let Ok(event) = WindowEvent::from_bytes(bytes) {
        let redecoded = WindowEvent::from_bytes(&event.to_le_bytes())
            .expect("round-trip of an accepted event must succeed");
        assert_eq!(event, redecoded);
    }
    let _ = decode_create_reply(bytes);
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
    }
    if let Ok(header) = SysinfoRequestHeader::from_bytes(bytes) {
        let redecoded = SysinfoRequestHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    exercise_users_admin(bytes);
    exercise_sysinfo_records(bytes);
    exercise_seatmgr(bytes);
    exercise_display_ipc(bytes);
    exercise_window_ipc(bytes);
    exercise_net_socket(bytes);
    exercise_net_channel(bytes);
    exercise_elevate(bytes);
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

    let Ok(len) = tairix_abi::process::encoded_len(args, env) else {
        return;
    };
    let mut buf = vec![0u8; len];
    let Ok(written) = tairix_abi::process::write_into(&mut buf, args, env, canary) else {
        // A rejected build (an embedded NUL, say) is a fail-closed outcome.
        return;
    };
    assert_eq!(written, len);
    let view = ProcessStart::parse(&buf).expect("a freshly built block must parse");
    assert_eq!(view.arg_count() as usize, args.len());
    assert_eq!(view.env_count() as usize, env.len());
    assert_eq!(view.canary(), canary);
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
fn structured_fs_inputs_with_corrupted_fields_never_panic() {
    // Walk the accepted/rejected boundary of the filesystem decoders from
    // well-formed images: a `FileStat` and a packed `DirEntry`. Bit-flipping
    // each byte drives the kind/length/reserved-field checks without ever
    // panicking.
    let mut stat = [0u8; FileStat::WIRE_LEN];
    FileStat {
        kind: FileKind::Regular,
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
