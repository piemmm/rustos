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
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, ORIGIN_WIRE_LEN};
    use tairix_audio::stream::{AudioTransport, StreamClient};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_rt::shm::SharedRegion;
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

    /// Wait-set token for the stream's notify mailbox.
    const NOTIFY_TOKEN: u64 = 1;

    /// How long one park may wait for the next drain event before the run is
    /// declared stalled. Generous: an emulated machine clocks a period far
    /// slower than the hardware would, and this bounds a wedged run rather
    /// than pacing a healthy one.
    const DRAIN_WAIT_NS: u64 = 10_000_000_000;

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
    fn arm(transport: &mut RtAudio) -> Result<(StreamClient, SharedRegion, PcmGeometry), i32> {
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
        let region = SharedRegion::create(geometry.region_len()).ok_or_else(|| {
            fail(
                "the PCM ring could not be created",
                Errno::OutOfMemory,
                NO_REGION,
            )
        })?;
        let handle = tairix_rt::shm_grant(region.id(), AUDIO_ENDPOINT);
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
        let (mut stream, mut region, geometry) = match arm(&mut transport) {
            Ok(armed) => armed,
            Err(code) => return code,
        };
        // Queue the whole stream *before* the device is clocked: a device
        // that cannot run dry makes the capture exact. The signal is
        // followed by its silent tail, so the signal has left the host
        // backend's buffer before the stream ends.
        let mut samples = vec![0u8; wire::STREAM_FRAMES * wire::FRAME_BYTES];
        if wire::fill_signal(&mut samples) != wire::SIGNAL_FRAMES * wire::FRAME_BYTES {
            return fail(
                "the signal did not fit its buffer",
                Errno::BufferTooSmall,
                NOT_PLAYED,
            );
        }
        let mut ring = match PcmRing::bind(region.bytes_mut(), geometry) {
            Ok(ring) => ring,
            Err(err) => return fail("the ring would not bind", err, NO_REGION),
        };
        match stream.write_at(&mut ring, Frames::ZERO, &samples) {
            Ok(written)
                if written.sample_frames as usize == wire::STREAM_FRAMES
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
        // completed. An empty mailbox is `WouldBlock`, so the wait is the
        // wait set — re-reading the port in a loop would spin through the
        // whole budget without ever giving the service a chance to run.
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return fail(
                "no wait set for the drain",
                Errno::NotImplemented,
                NOT_PLAYED,
            );
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            transport.notify_port,
            NOTIFY_TOKEN,
        ) != 0
        {
            return fail(
                "the notify port would not join the wait set",
                Errno::NotImplemented,
                NOT_PLAYED,
            );
        }
        let mut frame = [0u8; AUDIO_NOTIFY_LEN];
        for _ in 0..MAX_WAKES {
            let mut token = 0u64;
            if tairix_rt::waitset_wait(set, DRAIN_WAIT_NS, &mut token) != 0 {
                break;
            }
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
        // Close what was opened, before claiming success: the endpoint is
        // released only when its last stream goes, and a device told to
        // release is a device that has finished with the frames rather than
        // one still holding some.
        if let Err(err) = stream.close(&mut transport) {
            return fail("the stream would not close", err, NOT_PLAYED);
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

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
#[cfg(not(freestanding))]
fn main() {}
