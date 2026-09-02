//! The grid's icon-artwork pipeline: the decode cache a paint resolves
//! through, and the one decode per turn the event loop runs.
//!
//! One tile's icon costs a bounded VFS read plus a round trip to the parser
//! sandbox. Inside a paint that is one round trip per visible tile on a
//! folder's first frame and another per row revealed by a scroll — seconds of
//! frozen window on a store like `/System/Commands`, a 256-square master per
//! bundle. So the paint neither reads nor decodes: it resolves through the
//! shared deferred-decode desk ([`ArtworkDesk`]), which answers what has
//! already been produced and *records* everything else, and a tile with
//! nothing yet draws its built-in glyph.
//!
//! The desktop session drives the same desk from a worker thread; this app
//! pumps it from its own loop. One window's grid is a bounded set of tiles and
//! the loop has nothing else to block on, so a thread and its stack would buy
//! only what a turn of the loop already interleaves.
//!
//! The cache, the reader, and the rasteriser are injected — only the running
//! program knows the window's frame size, the live pressure gauge, the audit
//! sink, and the capability the read runs under — so every rule below is a
//! host test.

use tairix_icon::{
    render_artwork, ArtworkCache, ArtworkDesk, ArtworkRasteriser, ArtworkReader, IconArtworkSource,
};

/// The decode cache, the desk that defers a miss, and the seams one decode
/// runs through.
pub struct IconPipeline<R: ArtworkReader, D: ArtworkRasteriser> {
    /// The retained decode outcomes, keyed by what was resolved and the pixel
    /// side.
    cache: ArtworkCache,
    /// What the paints have asked for and what has come back.
    desk: ArtworkDesk,
    /// Where an asset's bytes come from.
    reader: R,
    /// Where those bytes become pixels — the parser sandbox in production.
    rasteriser: D,
}

impl<R: ArtworkReader, D: ArtworkRasteriser> IconPipeline<R, D> {
    /// A pipeline over a ready-built `cache` and the seams a decode runs
    /// through.
    pub const fn new(cache: ArtworkCache, reader: R, rasteriser: D) -> Self {
        Self {
            cache,
            desk: ArtworkDesk::new(),
            reader,
            rasteriser,
        }
    }

    /// The lookup a paint is handed: the cache bound to the desk, so a miss is
    /// recorded rather than read.
    pub fn source(&mut self) -> IconArtworkSource<'_> {
        IconArtworkSource::new(&mut self.cache, &mut self.desk)
    }

    /// Run one recorded decode, reporting whether there was one to run.
    ///
    /// One per call, so queued input is served between decodes and a folder of
    /// fifty bundles never holds the window: the loop drains its mailbox, runs
    /// a decode, repaints what landed, and parks once both are exhausted.
    pub fn pump(&mut self) -> bool {
        let Some(job) = self.desk.next_job() else {
            return false;
        };
        // The read and the sandbox round trip: the calls that used to be
        // inside the paint. The decode is the shared one, so deferring it
        // cannot change what a tile draws.
        let artwork = render_artwork(&mut self.reader, &mut self.rasteriser, &job.key, job.side);
        // Single-threaded, so nothing can have taken the slot since
        // `next_job`; the loop reads `take_landed` for whether a frame is owed.
        self.desk.deliver(&job, artwork);
        true
    }

    /// Whether a decode has landed since this was last asked.
    ///
    /// The loop repaints on a `true`, so a pump that delivered nothing new
    /// costs no frame.
    pub fn take_landed(&mut self) -> bool {
        self.desk.take_landed()
    }

    /// Give back what a changed memory-pressure band no longer allows,
    /// answering the bytes released.
    ///
    /// The band that refused to keep a decode may now allow it, so the
    /// decision is remade here, on the band's own wake, rather than by every
    /// repaint in between.
    pub fn trim(&mut self) -> usize {
        let released = self.cache.trim();
        self.desk.retry_declined();
        released
    }
}

impl<R: ArtworkReader, D: ArtworkRasteriser> Drop for IconPipeline<R, D> {
    /// Release every retained decode and every answer the desk still holds,
    /// overwriting the pixels first, so one user's decoded artwork never
    /// outlives their window in reusable heap — on every way out of the app,
    /// not the ones a future edit remembers to spell.
    fn drop(&mut self) {
        self.desk.stop();
        self.cache.teardown();
    }
}

#[cfg(test)]
#[path = "icons_tests.rs"]
mod tests;
