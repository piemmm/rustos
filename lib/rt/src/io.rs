//! Ergonomic userland I/O over the `abi-v1` descriptor table.
//!
//! This is the TAIRiX counterpart of `std::io`: one fd-generic
//! [`Read`]/[`Write`] trait pair, the buffering ([`BufReader`], [`BufWriter`])
//! and formatting built on them, and the four well-known standard streams
//! ([`Stdin`], [`Stdout`], [`Stderr`], [`StdInfo`]). It is a pure-Rust layer
//! over the existing `stream_read` / `stream_write` traps: it adds **no**
//! syscall, capability, or `lib/abi` type, and reaches no authority a program
//! does not already hold — an I/O object only ever names a descriptor the
//! kernel already gave the process, never a device.
//!
//! # One vocabulary, no duplication
//!
//! Every text program in TAIRiX moves bytes through this one definition, so no
//! program re-implements the short-write loop or "read until newline" logic.
//! The standard streams and any pipe / tty / file / resource-reference fd a
//! sibling subsystem later opens are read and written through the **same**
//! traits: a [`Stream`] over an arbitrary fd shares the identical code path as
//! [`Stdout`].
//!
//! # Not the opener
//!
//! Obtaining a *new* fd (opening a file under a capability, resolving a
//! resource reference) is a capability-bearing operation owned by the
//! filesystem and resource-reference subsystems, not this layer. This module
//! constructs only the four inherited standard streams on its own; any other
//! fd is *handed in* by the owning subsystem's open/resolve call. It therefore
//! exposes no `open`/`resolve` and cannot widen authority.
//!
//! There is deliberately no owning, close-on-drop stream handle here yet:
//! `abi-v1` has no generic descriptor-close trap (only the filesystem's own
//! `File`, which closes via `fs_close` on drop), so an fd-generic RAII closer
//! would be a speculative interface bound to a syscall that does not exist. It
//! lands with the descriptor-producing/closing ABI that will own it.
//!
//! # Fail closed
//!
//! No path panics or uses `unwrap`/`expect`. A short read or write is a value
//! the provided helpers loop over, end-of-input is reported honestly as a
//! zero-length read, and a formatting failure surfaces as [`Error::Fmt`].

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{STDERR, STDIN, STDINFO, STDOUT};

/// The error type for this layer. Small and fail-closed; `abi-v1` is not
/// frozen, so it is extended in place as real callers need distinctions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A [`Write::write`] reported zero bytes accepted while bytes still
    /// remained to be written, so [`Write::write_all`] cannot make progress.
    /// Reported rather than looped forever (fail closed).
    WriteZero,
    /// A line read by [`BufReader::read_line`] was not valid UTF-8.
    InvalidUtf8,
    /// A [`Read::read_exact`] hit end-of-input before filling the buffer.
    UnexpectedEof,
    /// A [`core::fmt`] formatting implementation returned an error while
    /// rendering into a stream (surfaced instead of panicking).
    Fmt,
}

/// The result type for this layer.
pub type Result<T> = core::result::Result<T, Error>;

/// A source of bytes read from a stream descriptor.
///
/// Implementors provide only the primitive [`read`](Read::read); the looping
/// helpers ([`read_exact`](Read::read_exact)) are provided so callers never
/// re-implement the short-read loop.
pub trait Read {
    /// Read some bytes into `buf`, returning how many were read.
    ///
    /// A return of `0` means end-of-input (or, for the standard streams, that
    /// the backing reported a failure it cannot express as an `Errno`). A
    /// short read (fewer than `buf.len()`) is normal; the caller loops.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Read exactly `buf.len()` bytes, looping over short reads.
    ///
    /// # Errors
    ///
    /// [`Error::UnexpectedEof`] if end-of-input is reached before `buf` is
    /// filled (fail closed, never an infinite loop).
    fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.read(buf)? {
                0 => return Err(Error::UnexpectedEof),
                n => buf = &mut buf[n..],
            }
        }
        Ok(())
    }
}

/// A sink for bytes written to a stream descriptor.
///
/// Implementors provide only the primitive [`write`](Write::write) (and
/// optionally [`flush`](Write::flush)); the looping [`write_all`](Write::write_all)
/// and the [`write_fmt`](Write::write_fmt) formatting bridge are provided so
/// callers never re-implement the short-write loop.
pub trait Write {
    /// Write some bytes from `buf`, returning how many were accepted.
    ///
    /// A short write (fewer than `buf.len()`) is normal; the caller loops. A
    /// return of `0` with bytes still pending is turned into
    /// [`Error::WriteZero`] by [`write_all`](Write::write_all) rather than
    /// spinning.
    fn write(&mut self, buf: &[u8]) -> Result<usize>;

    /// Flush any buffered bytes to the underlying stream. The unbuffered
    /// streams write straight through, so the default is a no-op.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Write all of `buf`, looping over short writes.
    ///
    /// # Errors
    ///
    /// [`Error::WriteZero`] if the stream stops accepting bytes before `buf`
    /// is fully written (fail closed, never an infinite loop).
    fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.write(buf)? {
                0 => return Err(Error::WriteZero),
                n => buf = &buf[n..],
            }
        }
        Ok(())
    }

    /// Write formatted output (`write!` / `writeln!` support).
    ///
    /// # Errors
    ///
    /// [`Error::Fmt`] if a `Display`/`Debug` implementation fails or the
    /// underlying [`write_all`](Write::write_all) does.
    fn write_fmt(&mut self, args: core::fmt::Arguments<'_>) -> Result<()> {
        let mut adapter = FmtAdapter {
            inner: self,
            error: None,
        };
        match core::fmt::write(&mut adapter, args) {
            Ok(()) => Ok(()),
            Err(_) => Err(adapter.error.unwrap_or(Error::Fmt)),
        }
    }
}

/// Bridges [`core::fmt::Write`] onto a [`Write`], capturing the first I/O
/// error so a formatting failure surfaces as a typed [`Error`] rather than the
/// opaque [`core::fmt::Error`].
struct FmtAdapter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    error: Option<Error>,
}

impl<W: Write + ?Sized> core::fmt::Write for FmtAdapter<'_, W> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        match self.inner.write_all(s.as_bytes()) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.error = Some(e);
                Err(core::fmt::Error)
            }
        }
    }
}

/// An fd-generic, **non-owning** view of a stream descriptor.
///
/// A `Stream` names a descriptor the process already owns (a standard stream,
/// or a pipe / tty / resource-reference fd a spawner wired in) and moves bytes
/// over the shared `stream_read` / `stream_write` primitives — the identical
/// code path the standard streams use. It does **not** own or close the fd:
/// `abi-v1` has no generic descriptor-close trap, and lifetime is the concern
/// of whichever subsystem minted the fd (the filesystem's `File` closes its
/// own descriptor on drop). Constructing a `Stream` grants no authority: the
/// kernel resolves the fd against the caller's descriptor table on every call
/// and rejects one the process does not hold.
#[derive(Debug, Clone, Copy)]
pub struct Stream {
    fd: u32,
}

impl Stream {
    /// View descriptor `fd` as a stream. Non-owning: the caller (or the
    /// subsystem that opened `fd`) remains responsible for its lifetime.
    #[must_use]
    pub const fn new(fd: u32) -> Self {
        Self { fd }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        Ok(crate::stream_read(self.fd, buf))
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        Ok(crate::stream_write(self.fd, buf))
    }
}

/// Standard input (fd 0): the program's primary text input.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stdin;

/// Standard output (fd 1): the program's primary data output.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stdout;

/// Standard error (fd 2): errors, warnings, and diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stderr;

/// The standard information stream (fd 3): optional, ignorable structured
/// advisory metadata. Writes are best-effort and must never affect
/// correctness, so its [`Write`] never reports a short write or an error.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdInfo;

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        Ok(crate::stream_read(STDIN, buf))
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        Ok(crate::stream_write(STDOUT, buf))
    }
}

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        Ok(crate::stream_write(STDERR, buf))
    }
}

impl Write for StdInfo {
    /// Emit `buf` to fd 3 best-effort and report it fully consumed regardless
    /// of how many bytes the kernel accepted. fd 3 is ignorable (there may be
    /// no consumer), so it must never surface a short write that would stall
    /// [`Write::write_all`] or turn into an error a program depends on.
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let _ = crate::stream_write(STDINFO, buf);
        Ok(buf.len())
    }
}

/// Write `line` and a trailing newline to standard error (fd 2), best-effort.
///
/// A tool routes a diagnostic (a usage banner, a failed-query message) here so
/// it never contaminates the standard-output data stream. Shared by every
/// command app's `Run` binary, never re-derived per tool. Best-effort: a
/// stream that accepts no more bytes ends the write rather than spinning, so
/// the fail-closed result is discarded.
pub fn write_stderr_line(line: &str) {
    let mut err = Stderr;
    let _ = err.write_all(line.as_bytes());
    let _ = err.write_all(b"\n");
}

/// Default fixed capacity of a [`BufReader`] / [`BufWriter`] buffer, in bytes.
pub const DEFAULT_BUF_CAPACITY: usize = 4096;

/// A bounded, allocation-free [`core::fmt::Write`] sink over a fixed inline
/// buffer.
///
/// Formatting past the buffer's end truncates at a UTF-8 character boundary
/// rather than allocating, failing, or tearing a scalar, so a caller with a
/// fixed byte budget can format safely where the heap may be unavailable.
/// The runtime's panic handler formats its report through this: a panic may
/// itself be an out-of-memory condition, so that path must never allocate.
pub struct FixedFmtBuf<const CAP: usize> {
    bytes: [u8; CAP],
    len: usize,
}

impl<const CAP: usize> FixedFmtBuf<CAP> {
    /// An empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAP],
            len: 0,
        }
    }

    /// The bytes formatted so far (always valid UTF-8).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl<const CAP: usize> Default for FixedFmtBuf<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize> core::fmt::Write for FixedFmtBuf<CAP> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = CAP.saturating_sub(self.len);
        let mut take = s.len().min(room);
        while take > 0 && !s.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// A [`Write`] that coalesces many small writes into a single underlying write.
///
/// The buffer is a fixed-capacity inline array (`CAP` bytes, no heap
/// allocation), so a program emitting output byte-by-byte does not issue one
/// syscall per byte. Buffered bytes are flushed when the buffer fills, on an
/// explicit [`flush`](Write::flush), and best-effort on drop. A write larger
/// than the buffer bypasses it and goes straight to the inner stream.
#[derive(Debug)]
pub struct BufWriter<W: Write, const CAP: usize = DEFAULT_BUF_CAPACITY> {
    inner: W,
    buf: [u8; CAP],
    len: usize,
}

impl<W: Write, const CAP: usize> BufWriter<W, CAP> {
    /// Wrap `inner` with a fixed-capacity write buffer.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: [0; CAP],
            len: 0,
        }
    }

    /// A shared reference to the wrapped stream (does not flush).
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Write the buffered bytes to the inner stream and empty the buffer.
    fn flush_buf(&mut self) -> Result<()> {
        if self.len > 0 {
            self.inner.write_all(&self.buf[..self.len])?;
            self.len = 0;
        }
        Ok(())
    }
}

impl<W: Write, const CAP: usize> Write for BufWriter<W, CAP> {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        if self.len + data.len() > CAP {
            self.flush_buf()?;
        }
        if data.len() >= CAP {
            // Too large to buffer usefully: write straight through so a bulk
            // write is not split across buffer-sized syscalls.
            self.inner.write(data)
        } else {
            let end = self.len + data.len();
            self.buf[self.len..end].copy_from_slice(data);
            self.len = end;
            Ok(data.len())
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_buf()?;
        self.inner.flush()
    }
}

impl<W: Write, const CAP: usize> Drop for BufWriter<W, CAP> {
    fn drop(&mut self) {
        // Best-effort: a program that cares about the outcome calls `flush`
        // explicitly and handles the error. A drop-time failure has nowhere to
        // be reported, so it is dropped rather than panicking (fail closed).
        let _ = self.flush_buf();
    }
}

/// A [`Read`] that buffers the underlying stream so a reader is not one syscall
/// per byte, and offers line-oriented reading for a REPL.
///
/// The buffer is a fixed-capacity inline array (`CAP` bytes, no heap
/// allocation for the buffer itself); [`read_until`](BufReader::read_until) and
/// [`read_line`](BufReader::read_line) accumulate into a caller-provided
/// growable buffer.
#[derive(Debug)]
pub struct BufReader<R: Read, const CAP: usize = DEFAULT_BUF_CAPACITY> {
    inner: R,
    buf: [u8; CAP],
    pos: usize,
    cap: usize,
}

impl<R: Read, const CAP: usize> BufReader<R, CAP> {
    /// Wrap `inner` with a fixed-capacity read buffer.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: [0; CAP],
            pos: 0,
            cap: 0,
        }
    }

    /// Return the buffered bytes, refilling from the inner stream if empty.
    ///
    /// An empty returned slice means end-of-input.
    fn fill_buf(&mut self) -> Result<&[u8]> {
        if self.pos >= self.cap {
            self.cap = self.inner.read(&mut self.buf)?;
            self.pos = 0;
        }
        Ok(&self.buf[self.pos..self.cap])
    }

    /// Mark `amt` buffered bytes as consumed.
    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.cap);
    }

    /// Read bytes up to and including the first `delim` into `out`, returning
    /// the number of bytes appended. Stops early at end-of-input, in which
    /// case the returned count reflects the bytes read before it (and may not
    /// end with `delim`). Returns `0` only at end-of-input with nothing read.
    ///
    /// # Errors
    ///
    /// Propagates a [`Read::read`] error from the inner stream.
    pub fn read_until(&mut self, delim: u8, out: &mut Vec<u8>) -> Result<usize> {
        let mut total = 0;
        loop {
            let (done, used) = {
                let available = self.fill_buf()?;
                if available.is_empty() {
                    return Ok(total);
                }
                if let Some(i) = available.iter().position(|&b| b == delim) {
                    out.extend_from_slice(&available[..=i]);
                    (true, i + 1)
                } else {
                    out.extend_from_slice(available);
                    (false, available.len())
                }
            };
            self.consume(used);
            total += used;
            if done {
                return Ok(total);
            }
        }
    }

    /// Read one `\n`-terminated line into `out`, returning the number of bytes
    /// appended (including the newline). Returns `0` at end-of-input.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidUtf8`] if the line is not valid UTF-8, or a
    /// [`Read::read`] error from the inner stream.
    pub fn read_line(&mut self, out: &mut String) -> Result<usize> {
        let mut bytes = Vec::new();
        let n = self.read_until(b'\n', &mut bytes)?;
        match core::str::from_utf8(&bytes) {
            Ok(text) => {
                out.push_str(text);
                Ok(n)
            }
            Err(_) => Err(Error::InvalidUtf8),
        }
    }

    /// Consume the reader, yielding an iterator over its `\n`-separated lines.
    ///
    /// Each yielded line has its trailing `\n` (and a preceding `\r`, if any)
    /// stripped. Iteration ends at end-of-input; a read or decode error is
    /// yielded once as an [`Err`] item.
    pub fn lines(self) -> Lines<R, CAP> {
        Lines { reader: self }
    }
}

impl<R: Read, const CAP: usize> Read for BufReader<R, CAP> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

/// Iterator over the lines of a [`BufReader`], created by
/// [`BufReader::lines`].
#[derive(Debug)]
pub struct Lines<R: Read, const CAP: usize = DEFAULT_BUF_CAPACITY> {
    reader: BufReader<R, CAP>,
}

impl<R: Read, const CAP: usize> Iterator for Lines<R, CAP> {
    type Item = Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                Some(Ok(line))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt;
    use tairix_abi_trap::seam;

    /// A [`Write`] that records everything written but accepts at most `max`
    /// bytes per call, so the short-write loop in [`Write::write_all`] is
    /// exercised. `max == 0` models a stalled sink (drives [`Error::WriteZero`]).
    struct ChunkWriter {
        data: Vec<u8>,
        max: usize,
    }

    impl ChunkWriter {
        fn new(max: usize) -> Self {
            Self {
                data: Vec::new(),
                max,
            }
        }
    }

    impl Write for ChunkWriter {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            let n = buf.len().min(self.max);
            self.data.extend_from_slice(&buf[..n]);
            Ok(n)
        }
    }

    /// A [`Read`] that yields at most `max` bytes per call, so the short-read
    /// loop in [`Read::read_exact`] and the accumulation in
    /// [`BufReader::read_until`] are exercised.
    struct ChunkReader {
        data: Vec<u8>,
        pos: usize,
        max: usize,
    }

    impl ChunkReader {
        fn new(data: &[u8], max: usize) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
                max,
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.len()).min(self.max);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// A [`Write`] that records into a caller-visible shared buffer, so a
    /// `BufWriter`'s drop-time flush can be observed after the writer it owns
    /// has itself been dropped.
    struct SharedWriter {
        data: alloc::rc::Rc<core::cell::RefCell<Vec<u8>>>,
    }

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            self.data.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    /// A `Display` that always fails, to prove a formatting error surfaces as
    /// [`Error::Fmt`] rather than a panic.
    struct FailingDisplay;

    impl fmt::Display for FailingDisplay {
        fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
            Err(fmt::Error)
        }
    }

    #[test]
    fn write_all_loops_over_short_writes_to_full_length() {
        let mut w = ChunkWriter::new(3);
        w.write_all(b"hello world").expect("write_all completes");
        assert_eq!(w.data, b"hello world");
    }

    #[test]
    fn write_all_fails_closed_when_the_sink_stalls() {
        let mut w = ChunkWriter::new(0);
        assert_eq!(w.write_all(b"x"), Err(Error::WriteZero));
    }

    #[test]
    fn read_exact_loops_over_short_reads() {
        let mut r = ChunkReader::new(b"abcd", 2);
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf).expect("read_exact fills the buffer");
        assert_eq!(&buf, b"abcd");
    }

    #[test]
    fn read_exact_reports_unexpected_eof() {
        let mut r = ChunkReader::new(b"ab", 2);
        let mut buf = [0u8; 4];
        assert_eq!(r.read_exact(&mut buf), Err(Error::UnexpectedEof));
    }

    #[test]
    fn write_fmt_renders_formatted_output() {
        let mut w = ChunkWriter::new(usize::MAX);
        write!(w, "{}+{}={}", 2, 2, 4).expect("formatting succeeds");
        assert_eq!(w.data, b"2+2=4");
    }

    #[test]
    fn write_fmt_surfaces_a_formatting_error_as_fmt() {
        let mut w = ChunkWriter::new(usize::MAX);
        let failing = FailingDisplay;
        assert_eq!(write!(w, "{failing}"), Err(Error::Fmt));
    }

    #[test]
    fn bufwriter_coalesces_until_flush() {
        let mut bw = BufWriter::<_, 8>::new(ChunkWriter::new(usize::MAX));
        bw.write_all(b"abc").expect("buffered");
        bw.write_all(b"de").expect("buffered");
        assert!(bw.get_ref().data.is_empty(), "nothing reaches the sink yet");
        bw.flush().expect("flush drains the buffer");
        assert_eq!(bw.get_ref().data, b"abcde");
    }

    #[test]
    fn bufwriter_flushes_when_a_write_would_overflow_the_buffer() {
        let mut bw = BufWriter::<_, 4>::new(ChunkWriter::new(usize::MAX));
        bw.write_all(b"abc").expect("buffered below capacity");
        assert!(bw.get_ref().data.is_empty());
        bw.write_all(b"de")
            .expect("overflow flushes the buffer then buffers");
        assert_eq!(bw.get_ref().data, b"abc");
        bw.flush().expect("flush");
        assert_eq!(bw.get_ref().data, b"abcde");
    }

    #[test]
    fn bufwriter_passes_an_oversized_write_through_untorn() {
        let mut bw = BufWriter::<_, 4>::new(ChunkWriter::new(usize::MAX));
        bw.write_all(b"abcdef").expect("bulk write");
        assert_eq!(bw.get_ref().data, b"abcdef");
    }

    #[test]
    fn bufwriter_flushes_on_drop() {
        let sink = alloc::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
        {
            let mut bw = BufWriter::<_, 16>::new(SharedWriter { data: sink.clone() });
            bw.write_all(b"tail").expect("buffered");
            // No explicit flush: Drop must drain the buffer.
        }
        assert_eq!(*sink.borrow(), b"tail");
    }

    #[test]
    fn bufreader_read_line_accumulates_across_short_reads() {
        let mut r = BufReader::<_, 4>::new(ChunkReader::new(b"line1\nline2\n", 3));
        let mut line = String::new();
        assert_eq!(r.read_line(&mut line).expect("first line"), 6);
        assert_eq!(line, "line1\n");
        line.clear();
        assert_eq!(r.read_line(&mut line).expect("second line"), 6);
        assert_eq!(line, "line2\n");
        line.clear();
        assert_eq!(r.read_line(&mut line).expect("eof"), 0);
        assert!(line.is_empty());
    }

    #[test]
    fn bufreader_lines_strips_terminators() {
        let r = BufReader::<_, 4>::new(ChunkReader::new(b"a\r\nb\nc", 8));
        let lines: Vec<String> = r.lines().map(|l| l.expect("line")).collect();
        assert_eq!(lines, ["a", "b", "c"]);
    }

    #[test]
    fn bufreader_read_line_rejects_invalid_utf8() {
        let mut r = BufReader::<_, 8>::new(ChunkReader::new(b"\xff\n", 8));
        let mut line = String::new();
        assert_eq!(r.read_line(&mut line), Err(Error::InvalidUtf8));
    }

    #[test]
    fn stdout_and_a_plain_stream_share_one_write_path() {
        // Standard output marshals STREAM_WRITE with fd 1.
        seam::arm(6);
        assert_eq!(Stdout.write(b"hello\n").expect("write"), 6);
        let (number, args) = seam::last_call().expect("one trap");
        assert_eq!(number, crate::NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(STDOUT));

        // A `Stream` over an arbitrary fd takes the identical path with its fd.
        seam::arm(4);
        assert_eq!(Stream::new(7).write(b"data").expect("write"), 4);
        let (number, args) = seam::last_call().expect("one trap");
        assert_eq!(number, crate::NUM_STREAM_WRITE);
        assert_eq!(args[0], 7);
    }

    #[test]
    fn stderr_marshals_fd_two() {
        seam::arm(3);
        assert_eq!(Stderr.write(b"err").expect("write"), 3);
        let (number, args) = seam::last_call().expect("one trap");
        assert_eq!(number, crate::NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(STDERR));
    }

    #[test]
    fn write_all_fails_closed_on_a_refused_or_bogus_kernel_count() {
        // Regression test: a refused write (`-errno`, e.g. a missing
        // `CAP_CONSOLE_WRITE`) folds to a zero-length write, so `write_all`
        // reports `WriteZero` instead of slicing out of bounds — previously
        // the raw negative register became a huge count and `&buf[n..]`
        // panicked here, turning a panic report into a panic storm.
        let neg = u64::from_ne_bytes(
            (-i64::from(tairix_abi::Errno::PermissionDenied.as_i32())).to_ne_bytes(),
        );
        seam::arm(neg);
        assert_eq!(Stderr.write_all(b"report"), Err(Error::WriteZero));

        // A positive count larger than the written buffer (a buggy or
        // cross-delivered kernel count) is clamped to the buffer length, so
        // the loop completes instead of panicking.
        seam::arm(93);
        Stderr
            .write_all(b"tail")
            .expect("a clamped over-count completes the write loop");
    }

    #[test]
    fn stdin_reads_through_the_shared_read_path() {
        seam::arm(3);
        let mut buf = [0u8; 8];
        assert_eq!(Stdin.read(&mut buf).expect("read"), 3);
        let (number, args) = seam::last_call().expect("one trap");
        assert_eq!(number, crate::NUM_STREAM_READ);
        assert_eq!(args[0], u64::from(STDIN));
    }

    #[test]
    fn stdinfo_is_best_effort_and_never_a_short_write() {
        // The kernel accepts zero bytes (no consumer attached), yet the write
        // reports the buffer fully consumed so `write_all` never stalls.
        seam::arm(0);
        assert_eq!(StdInfo.write(b"info").expect("write"), 4);
        StdInfo
            .write_all(b"advisory")
            .expect("write_all never stalls on fd 3");
        let (number, args) = seam::last_call().expect("one trap");
        assert_eq!(number, crate::NUM_STREAM_WRITE);
        assert_eq!(args[0], u64::from(STDINFO));
    }

    #[test]
    fn fixed_fmt_buf_formats_within_its_budget() {
        use fmt::Write as _;
        let mut buf = FixedFmtBuf::<32>::new();
        let (message, line) = ("boom", 7);
        write!(buf, "panic: {message} at main.rs:{line}").expect("infallible");
        assert_eq!(buf.as_bytes(), b"panic: boom at main.rs:7");
    }

    #[test]
    fn fixed_fmt_buf_truncates_at_capacity_without_failing() {
        use fmt::Write as _;
        let mut buf = FixedFmtBuf::<8>::new();
        // The overflow is truncated, never an error: the panic path must not
        // turn a long report into a second failure.
        write!(buf, "0123456789").expect("infallible");
        assert_eq!(buf.as_bytes(), b"01234567");
        // A full buffer keeps accepting (and dropping) further writes.
        write!(buf, "more").expect("infallible");
        assert_eq!(buf.as_bytes(), b"01234567");
    }

    #[test]
    fn fixed_fmt_buf_truncates_on_a_character_boundary() {
        use fmt::Write as _;
        // "éé" is four bytes; a 3-byte budget must keep one whole scalar and
        // drop the torn one, so the report stays valid UTF-8.
        let mut buf = FixedFmtBuf::<3>::new();
        write!(buf, "éé").expect("infallible");
        assert_eq!(buf.as_bytes(), "é".as_bytes());
        assert!(core::str::from_utf8(buf.as_bytes()).is_ok());
    }
}
