//! Bounded rolling "keep the last N units" windows, over bytes and over
//! delimited lines.
//!
//! Both the `head` and `tail` command apps (`plans/APPS.md` §12.1 Stage C)
//! need the identical primitive: a fixed-capacity window that retains only
//! the most recent `keep` units of a stream and hands every unit that ages
//! out to a caller-supplied sink, in order. `head -c -N` / `head -n -N`
//! (emit everything *but* the last N, dropping the retained window) and
//! `tail -c N` / `tail -n N` (drop the aged-out units, emit the retained
//! window at end) are the two policies over this one mechanism, so the
//! mechanism lives here once rather than being copied into each tool.
//!
//! # Memory is bounded by the window, never by the stream
//!
//! A [`ByteWindow`] holds at most `keep` bytes; a [`LineWindow`] holds at
//! most `keep` complete lines plus the growing partial line. Each pushed
//! unit is copied O(1) times. A stream of any length therefore flows
//! through a constant amount of memory, which is what lets both tools
//! stream inputs larger than memory.
//!
//! # Fail closed, never panic
//!
//! The push/drain methods thread the caller's error type `E` through
//! unchanged and never `unwrap`/`expect`/`panic!`. A
//! `keep` larger than any addressable buffer is clamped to [`usize::MAX`]
//! and behaves as "retain everything", which is exactly the semantics of
//! keeping/eliding more units than the input can hold.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// A rolling window of the most recent `keep` bytes of a stream.
///
/// Feed the stream in order with [`push`](ByteWindow::push); each call hands
/// the bytes that fall out of the window to the `aged` sink (oldest first).
/// [`drain`](ByteWindow::drain) then emits the bytes still retained, oldest
/// to newest. The window is stored circularly, so a full window drains in
/// at most two contiguous slices.
pub struct ByteWindow {
    keep: usize,
    buf: Vec<u8>,
    /// Index of the oldest byte once `buf` has filled to `keep`. Before the
    /// window fills, `buf` is a plain in-order prefix and `start` is `0`, so
    /// the same `split_at(start)` view yields the oldest run uniformly.
    start: usize,
}

impl ByteWindow {
    /// Create a window retaining the most recent `keep` bytes. A `keep`
    /// beyond [`usize::MAX`] is clamped and retains everything.
    #[must_use]
    pub fn new(keep: u64) -> Self {
        Self {
            keep: usize::try_from(keep).unwrap_or(usize::MAX),
            buf: Vec::new(),
            start: 0,
        }
    }

    /// Feed the next `chunk`, handing every byte that ages out of the
    /// window to `aged` in stream order.
    ///
    /// # Errors
    ///
    /// Returns the first error `aged` reports; the window is left consistent
    /// only up to that point, and no further work is attempted.
    pub fn push<E>(
        &mut self,
        chunk: &[u8],
        mut aged: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.keep == 0 {
            // Nothing is ever retained: every byte ages out immediately.
            return aged(chunk);
        }
        if chunk.len() >= self.keep {
            // The chunk alone covers the whole window: everything buffered,
            // and the chunk's own prefix, ages out at once. In circular
            // order the oldest bytes run from `start` to the end, then wrap
            // to the front.
            let (wrapped, oldest) = self.buf.split_at(self.start.min(self.buf.len()));
            aged(oldest)?;
            aged(wrapped)?;
            aged(&chunk[..chunk.len() - self.keep])?;
            self.buf.clear();
            self.buf
                .extend_from_slice(&chunk[chunk.len() - self.keep..]);
            self.start = 0;
            return Ok(());
        }
        if self.buf.len() < self.keep {
            let total = self.buf.len() + chunk.len();
            if total <= self.keep {
                self.buf.extend_from_slice(chunk);
                return Ok(());
            }
            // Filling-to-full transition: the front of the linear buffer
            // ages out, the remainder plus the chunk is exactly the window.
            let excess = total - self.keep;
            aged(&self.buf[..excess])?;
            let mut window = Vec::with_capacity(self.keep);
            window.extend_from_slice(&self.buf[excess..]);
            window.extend_from_slice(chunk);
            self.buf = window;
            self.start = 0;
            return Ok(());
        }
        // Full circular window: exactly `chunk.len()` oldest bytes age out
        // and the chunk overwrites their slots.
        let len = chunk.len();
        let first_span = len.min(self.keep - self.start);
        aged(&self.buf[self.start..self.start + first_span])?;
        if len > first_span {
            aged(&self.buf[..len - first_span])?;
        }
        self.buf[self.start..self.start + first_span].copy_from_slice(&chunk[..first_span]);
        if len > first_span {
            self.buf[..len - first_span].copy_from_slice(&chunk[first_span..]);
        }
        self.start = (self.start + len) % self.keep;
        Ok(())
    }

    /// Emit the bytes still retained in the window, oldest to newest, in at
    /// most two contiguous slices.
    ///
    /// # Errors
    ///
    /// Returns the first error `emit` reports.
    pub fn drain<E>(&self, mut emit: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let (wrapped, oldest) = self.buf.split_at(self.start.min(self.buf.len()));
        emit(oldest)?;
        emit(wrapped)?;
        Ok(())
    }
}

/// A rolling window of the most recent `keep` complete lines of a stream.
///
/// A "line" runs through and includes its `delim` byte; the unterminated
/// final fragment counts as a line once [`finish_partial`](LineWindow::finish_partial)
/// is called, exactly as GNU `head`/`tail` treat a missing trailing newline.
/// [`push`](LineWindow::push) hands each line that ages out of the window to
/// the `aged` sink; [`drain`](LineWindow::drain) emits the retained lines,
/// oldest to newest.
pub struct LineWindow {
    keep: usize,
    delim: u8,
    queue: VecDeque<Vec<u8>>,
    partial: Vec<u8>,
}

impl LineWindow {
    /// Create a window retaining the most recent `keep` lines delimited by
    /// `delim` (`b'\n'` normally, `0` for `-z`). A `keep` beyond
    /// [`usize::MAX`] is clamped and retains everything.
    #[must_use]
    pub fn new(keep: u64, delim: u8) -> Self {
        Self {
            keep: usize::try_from(keep).unwrap_or(usize::MAX),
            delim,
            queue: VecDeque::new(),
            partial: Vec::new(),
        }
    }

    /// Feed the next `chunk`, handing every line that ages out of the window
    /// to `aged` in stream order. A line split across chunks is reassembled.
    ///
    /// # Errors
    ///
    /// Returns the first error `aged` reports.
    pub fn push<E>(
        &mut self,
        chunk: &[u8],
        mut aged: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|&b| b == self.delim) {
            self.partial.extend_from_slice(&rest[..=pos]);
            let line = core::mem::take(&mut self.partial);
            self.queue.push_back(line);
            self.age_out(&mut aged)?;
            rest = &rest[pos + 1..];
        }
        self.partial.extend_from_slice(rest);
        Ok(())
    }

    /// Settle the trailing unterminated fragment as a final line (if any),
    /// aging a line out if that pushes the window over capacity.
    ///
    /// # Errors
    ///
    /// Returns the first error `aged` reports.
    pub fn finish_partial<E>(
        &mut self,
        mut aged: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.partial.is_empty() {
            let line = core::mem::take(&mut self.partial);
            self.queue.push_back(line);
            self.age_out(&mut aged)?;
        }
        Ok(())
    }

    /// Emit the lines still retained in the window, oldest to newest.
    ///
    /// # Errors
    ///
    /// Returns the first error `emit` reports.
    pub fn drain<E>(&self, mut emit: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        for line in &self.queue {
            emit(line)?;
        }
        Ok(())
    }

    /// Emit the oldest queued line once the queue exceeds `keep`. A push
    /// grows the queue by at most one line, so a single pop restores the
    /// invariant.
    fn age_out<E>(&mut self, aged: &mut impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        if self.queue.len() > self.keep {
            let aged_line = self.queue.pop_front().unwrap_or_default();
            aged(&aged_line)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{ByteWindow, LineWindow};

    /// Collect the aged-out bytes and then the retained window; the two
    /// together must reconstruct the original stream for every chunking.
    #[test]
    fn byte_window_partitions_the_stream_into_aged_then_retained() {
        let data: Vec<u8> = (0u16..251).map(|v| (v % 251) as u8).collect();
        for keep in [0usize, 1, 3, 7, 64, 250, 251, 300] {
            for chunk in [1usize, 2, 3, 5, 8, 63, 64, 65, 251] {
                let mut aged: Vec<u8> = Vec::new();
                let mut window = ByteWindow::new(keep as u64);
                for piece in data.chunks(chunk) {
                    window
                        .push::<()>(piece, |bytes| {
                            aged.extend_from_slice(bytes);
                            Ok(())
                        })
                        .expect("infallible sink");
                }
                let mut retained: Vec<u8> = Vec::new();
                window
                    .drain::<()>(|bytes| {
                        retained.extend_from_slice(bytes);
                        Ok(())
                    })
                    .expect("infallible sink");

                let split = data.len().saturating_sub(keep);
                assert_eq!(&aged, &data[..split], "keep={keep} chunk={chunk} aged part");
                assert_eq!(
                    &retained,
                    &data[split..],
                    "keep={keep} chunk={chunk} retained part"
                );
            }
        }
    }

    /// The retained window drains exactly the last `keep` bytes in order.
    #[test]
    fn byte_window_drain_is_the_last_keep_bytes() {
        let mut window = ByteWindow::new(3);
        let mut retained = Vec::new();
        window.push::<()>(b"hello world", |_| Ok(())).expect("sink");
        window
            .drain::<()>(|b| {
                retained.extend_from_slice(b);
                Ok(())
            })
            .expect("sink");
        assert_eq!(retained, b"rld");
    }

    /// Aged-out lines then retained lines reconstruct every line of the
    /// stream, for every chunking, with an unterminated tail counting as a
    /// line once settled.
    #[test]
    fn line_window_partitions_lines_including_the_unterminated_tail() {
        for terminated in [true, false] {
            let mut data: Vec<u8> = Vec::new();
            for i in 0..20u8 {
                data.push(b'a' + i);
                if i + 1 < 20 || terminated {
                    data.push(b'\n');
                }
            }
            for keep in [0usize, 1, 5, 19, 20, 25] {
                for chunk in [1usize, 2, 3, 7, 40] {
                    let mut aged: Vec<Vec<u8>> = Vec::new();
                    let mut window = LineWindow::new(keep as u64, b'\n');
                    for piece in data.chunks(chunk) {
                        window
                            .push::<()>(piece, |line| {
                                aged.push(line.to_vec());
                                Ok(())
                            })
                            .expect("sink");
                    }
                    window
                        .finish_partial::<()>(|line| {
                            aged.push(line.to_vec());
                            Ok(())
                        })
                        .expect("sink");
                    let mut retained: Vec<Vec<u8>> = Vec::new();
                    window
                        .drain::<()>(|line| {
                            retained.push(line.to_vec());
                            Ok(())
                        })
                        .expect("sink");

                    let mut all = aged.clone();
                    all.extend(retained.iter().cloned());
                    let mut rebuilt: Vec<u8> = Vec::new();
                    for line in &all {
                        rebuilt.extend_from_slice(line);
                    }
                    assert_eq!(rebuilt, data, "keep={keep} chunk={chunk} term={terminated}");
                    assert!(
                        retained.len() <= keep,
                        "keep={keep} chunk={chunk}: retained {} lines",
                        retained.len()
                    );
                }
            }
        }
    }

    /// A zero-length window retains nothing: every line ages out.
    #[test]
    fn line_window_zero_keep_retains_nothing() {
        let mut window = LineWindow::new(0, b'\n');
        let mut aged = 0usize;
        window
            .push::<()>(b"a\nb\nc\n", |_| {
                aged += 1;
                Ok(())
            })
            .expect("sink");
        let mut retained = 0usize;
        window
            .drain::<()>(|_| {
                retained += 1;
                Ok(())
            })
            .expect("sink");
        assert_eq!(aged, 3);
        assert_eq!(retained, 0);
    }
}
