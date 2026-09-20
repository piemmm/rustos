//! `audiotone` — the guest half of the end-to-end audio verticals
//! (`plans/SOUND.md` SND4).
//!
//! It plays the shared deterministic signal
//! ([`tairix_test_audio_wire`]) through the whole production stack — the
//! `audio-v1` client in `lib/audio`, the `audiod` mixer, the `audiochan-v1`
//! device channel, the autoloaded `virtio_snd` driver process, and QEMU's
//! emulated sound card — and QEMU's `wav` backend writes what the card
//! received to a file the harness then checks sample for sample.
//!
//! # Why it queues everything before starting
//!
//! The ring is sized to hold the whole signal, so every frame is written
//! *before* the device is clocked. The device therefore cannot run dry
//! however slowly the emulated machine runs: the capture is sample-exact
//! rather than "sample-exact modulo inserted silence", and the driver's
//! reported lost-frame tally is exactly zero rather than merely small. A
//! guest that raced the device would turn a real defect and a slow host into
//! the same observation.
//!
//! It never self-exits on a failure path other than by saying why: a
//! shortfall prints its reason on `stderr` and exits non-zero, so the run
//! fails loud rather than quietly passing.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    extern crate alloc;

    use alloc::vec;
    use core::fmt::Write as _;

    use tairix_abi::audio::{
        AudioNotify, OpenParams, StreamRole, StreamState, AUDIO_ENDPOINT, AUDIO_NOTIFY_LEN,
    };
    use tairix_abi::driver::audio::{ChannelMap, Frames, Rate, SampleFormat, StreamDirection};
    use tairix_abi::driver::audio_ring::{PcmGeometry, PcmRing};
    use tairix_abi::{Errno, ORIGIN_WIRE_LEN};
    use tairix_audio::stream::{AudioTransport, StreamClient};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_test_audio_wire as wire;

    /// Exit code when the audio service refused, or was not there.
    const NO_SERVICE: i32 = 70;
    /// Exit code when the shared ring could not be created or granted.
    const NO_REGION: i32 = 71;
    /// Exit code when the signal did not reach the device intact.
    const NOT_PLAYED: i32 = 72;

    /// Notifies to take before giving up on the drain completing.
    ///
    /// A bound rather than a timeout: each wake is a real service event, and
    /// a quarter-second signal at this device's period cannot need more than
    /// a few hundred of them. Exceeding it means the stack stopped making
    /// progress, which is a failure to report rather than to wait out.
    const MAX_WAKES: usize = 4_096;

    /// The live `audio-v1` transport: one `ipc_call` to the service's
    /// reserved rendezvous, and a parked receive on this stream's own notify
    /// mailbox.
    struct RtAudio {
        notify_port: u64,
    }

    impl AudioTransport for RtAudio {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(AUDIO_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }

        fn wait_notify(&mut self, out: &mut [u8]) -> Result<usize, Errno> {
            let mut from = [0u8; ORIGIN_WIRE_LEN];
            tairix_rt::ipc_recv(self.notify_port, out, &mut from).map_err(Errno::from_syscall)
        }
    }

    /// Say why the run failed on `stderr`, so an abnormal exit is never
    /// silent, and answer the exit code.
    fn fail(reason: &str, err: Errno, code: i32) -> i32 {
        let mut line = [0u8; 128];
        let mut cursor = Cursor {
            buf: &mut line,
            len: 0,
        };
        // Bounded, well-formed input — a short fixed reason plus a small
        // integer — so an overflow cannot occur; a refused write leaves the
        // prefix written, which still names the failure.
        let _ = write!(cursor, "audiotone: {reason} (errno {})", err.as_i32());
        let len = cursor.len;
        write_stderr_line(core::str::from_utf8(&line[..len]).unwrap_or("audiotone: failed"));
        code
    }

    /// A bounded `core::fmt::Write` sink over a fixed buffer; a write past
    /// the end is refused rather than truncating mid-character.
    struct Cursor<'a> {
        buf: &'a mut [u8; 128],
        len: usize,
    }

    impl core::fmt::Write for Cursor<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let end = self.len.checked_add(bytes.len()).ok_or(core::fmt::Error)?;
            if end > self.buf.len() {
                return Err(core::fmt::Error);
            }
            self.buf[self.len..end].copy_from_slice(bytes);
            self.len = end;
            Ok(())
        }
    }

    /// Open the stream, bind its notify port, and hand the service a ring
    /// the whole signal fits in.
    fn arm(transport: &mut RtAudio) -> Result<(StreamClient, Region, PcmGeometry), i32> {
        let Ok(rate) = Rate::new(wire::RATE_HZ) else {
            return Err(NO_SERVICE);
        };
        let params = OpenParams {
            // Zero names the machine's default sink; the vertical's guest has
            // exactly one.
            device_id: 0,
            direction: StreamDirection::Playback,
            format: SampleFormat::S16,
            rate,
            channel_map: ChannelMap::STEREO,
            role: StreamRole::Media,
            latency_target_frames: wire::RING_FRAMES,
        };
        let stream = StreamClient::open(transport, &params)
            .map_err(|err| fail("the audio service refused the stream", err, NO_SERVICE))?;
        let grant = stream.grant();
        // The notify port is derived by the service from this process's
        // kernel-attested pid and handed back in the grant, so the transport
        // learns it only now.
        transport.notify_port = grant.notify_endpoint;
        if tairix_rt::port_bind(grant.notify_endpoint, AUDIO_NOTIFY_LEN, 64) != 0 {
            return Err(fail(
                "the stream notify port would not bind",
                Errno::AddressInUse,
                NO_SERVICE,
            ));
        }
        let geometry = PcmGeometry::new(
            grant.ring_frames,
            grant.format,
            grant.channel_map.channels(),
        )
        .map_err(|err| fail("the granted ring has no shape", err, NO_REGION))?;
        let Some(region) = Region::create(geometry.region_len()) else {
            return Err(fail(
                "the PCM ring could not be created",
                Errno::OutOfMemory,
                NO_REGION,
            ));
        };
        let handle = tairix_rt::shm_grant(region.base, AUDIO_ENDPOINT);
        if handle < 0 {
            return Err(fail(
                "the ring could not be granted",
                Errno::from_syscall(handle),
                NO_REGION,
            ));
        }
        #[allow(clippy::cast_sign_loss)] // `handle >= 0` is the grant handle.
        stream
            .attach(transport, handle as u64)
            .map_err(|err| fail("the service refused the ring", err, NO_REGION))?;
        Ok((stream, region, geometry))
    }

    fn main() -> i32 {
        let mut transport = RtAudio { notify_port: 0 };
        let (mut stream, region, geometry) = match arm(&mut transport) {
            Ok(armed) => armed,
            Err(code) => return code,
        };
        // Queue the whole signal *before* the device is clocked: a device
        // that cannot run dry makes the capture exact.
        let mut samples = vec![0u8; wire::SIGNAL_FRAMES * wire::FRAME_BYTES];
        if wire::fill_signal(&mut samples) != samples.len() {
            return fail(
                "the signal did not fit its buffer",
                Errno::BufferTooSmall,
                NOT_PLAYED,
            );
        }
        let mut ring = match PcmRing::bind(region.bytes, geometry) {
            Ok(ring) => ring,
            Err(err) => return fail("the ring would not bind", err, NO_REGION),
        };
        match stream.write_at(&mut ring, Frames::ZERO, &samples) {
            Ok(written)
                if written.sample_frames as usize == wire::SIGNAL_FRAMES
                    && written.silence_frames == 0 => {}
            Ok(written) => {
                let _ = written;
                write_stderr_line("audiotone: the ring took only part of the signal");
                return NOT_PLAYED;
            }
            Err(err) => return fail("the ring refused the signal", err, NOT_PLAYED),
        }

        if let Err(err) = stream.start(&mut transport, Frames::ZERO) {
            return fail("the stream would not start", err, NOT_PLAYED);
        }
        if let Err(err) = stream.drain(&mut transport) {
            return fail("the stream would not drain", err, NOT_PLAYED);
        }

        // Park on the stream's own mailbox until the service says the drain
        // completed. Never a poll: each wake is a service event.
        let mut frame = [0u8; AUDIO_NOTIFY_LEN];
        for _ in 0..MAX_WAKES {
            let Ok(len) = transport.wait_notify(&mut frame) else {
                continue;
            };
            let Ok(notify) = AudioNotify::decode(&frame[..len]) else {
                continue;
            };
            stream.adopt(notify);
            if stream.state() == StreamState::Idle {
                break;
            }
        }
        let report = match stream.report(&mut transport) {
            Ok(report) => report,
            Err(err) => return fail("the stream state could not be read", err, NOT_PLAYED),
        };
        if report.state != StreamState::Idle {
            write_stderr_line("audiotone: the drain never completed");
            return NOT_PLAYED;
        }
        if report.xrun_frames != 0 {
            write_stderr_line("audiotone: frames were lost on a ring sized to hold them all");
            return NOT_PLAYED;
        }
        let mut marker = [0u8; 128];
        let mut cursor = Cursor {
            buf: &mut marker,
            len: 0,
        };
        let _ = writeln!(cursor, "{}", wire::PASS_MARKER);
        let len = cursor.len;
        if Stdout.write_all(&marker[..len]).is_err() {
            write_stderr_line("audiotone: the pass report could not be written");
            return NOT_PLAYED;
        }
        0
    }

    /// One anonymous shared region this process owns, mapped for its life.
    struct Region {
        base: u64,
        bytes: &'static mut [u8],
    }

    impl Region {
        /// Create and map a region of exactly `len` usable bytes.
        fn create(len: usize) -> Option<Self> {
            let mut handle = 0u64;
            if tairix_rt::shm_create(len, &mut handle) < 0 {
                return None;
            }
            let mut mapped_len = 0u64;
            let mapped = tairix_rt::shm_map(handle, &mut mapped_len);
            if mapped < 0 {
                return None;
            }
            let (base, addr, full) = (
                u64::try_from(mapped).ok()?,
                usize::try_from(mapped).ok()?,
                usize::try_from(mapped_len).ok()?,
            );
            if full < len {
                return None;
            }
            // SAFETY: `shm_map` mapped `full` bytes (>= `len`, checked above)
            // of zeroed, RW, non-executable memory at `addr`, owned by this
            // process for the rest of its life — the fixture never unmaps it.
            // The view covers the first `len` bytes, exactly the geometry the
            // grant describes, and nothing else in this address space aliases
            // them. The audio service maps the same frames through its own
            // grant; the ring's atomic positions order the two sides.
            let bytes = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) };
            Some(Self { base, bytes })
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
#[cfg(not(freestanding))]
fn main() {}
