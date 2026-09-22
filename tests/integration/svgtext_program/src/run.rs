//! The `Run` entry-point binary of the `svgtext` SVG-text fixture
//! (`plans/SVG.md` S23/S24).
//!
//! One binary, two roles. Re-entered with the seam's worker marker it is the
//! capability-empty decoder, serving the image-render service over its two
//! wired pipes exactly as the viewer's own worker does. Run as a command it
//! is the parent: it opens drawings through that sandbox against a live
//! `fontd` and measures what came back.
//!
//! # What only a running machine can show
//!
//! Every layer of the text path is host-tested, and the build-time icon
//! verification already drives the real service. What no host test reaches
//! is the **pipe between them**: a worker that holds no capability recording
//! the faces and scalars it could not answer, a parent fetching exactly
//! those from `fontd` over `FONT_ENDPOINT`, and a second decode drawing the
//! outlines that came back. That is the whole of what this fixture runs.
//!
//! # Why three renders
//!
//! Two drawings that differ in one character are rendered with the font
//! seam, and the wide one is rendered again through a sandbox given no seam
//! at all. The first two must ink differently in the direction their
//! characters do — nothing but real outlines does that — and the third must
//! be *refused*, which is what leaves the service as the only place the
//! outlines could have come from.
//!
//! Each render gets its own sandbox: a worker keeps the glyph table it was
//! supplied, so a second drawing through the same worker would never have to
//! ask and the seamless control would be answered from the first render's
//! table.
//!
//! A run that cannot render says so on `stderr` and on the system log under
//! [`REPORT_FAILED_EVENT`](tairix_test_svgtext::REPORT_FAILED_EVENT), and
//! exits non-zero — so a failure is loud at once rather than a run that
//! quietly waits out its budget.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

#[cfg(all(freestanding, feature = "program"))]
extern crate alloc;

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use alloc::vec;

    use tairix_font::ServiceFonts;
    use tairix_log::{log, Event, Field, FieldValue, Level, Sink};
    use tairix_raster::Region;
    use tairix_rt::io::write_stderr_line;
    use tairix_rt::LogSink;
    use tairix_sandbox::imagerender::{
        close_view, open_view, render_page, select_page, send_document, ImageRenderService,
        ViewFailure,
    };
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_svg::font::{FontProvider, NoFonts};
    use tairix_test_svgtext::{
        Report, REPORT_FAILED_EVENT, REPORT_FAILED_MESSAGE, REPORT_FAILED_STATUS,
    };

    /// Exit status of a worker whose serve loop failed on its transport.
    const WORKER_FAILED: i32 = 1;

    /// Bytes per straight-alpha RGBA8 pixel.
    const BYTES_PER_PIXEL: usize = 4;

    /// Why one render produced no pixels.
    enum RenderFailure {
        /// Refused for want of glyphs: the control's expected outcome, and
        /// a failure for every other render.
        FontsUnavailable,
        /// Anything else, naming the step that stopped it.
        Failed(&'static str),
    }

    /// One render's straight-alpha pixels and the extent they cover.
    struct Rendered {
        /// Destination pixels the render left non-transparent.
        ink: u64,
        /// Pixels in the render.
        pixels: u64,
    }

    /// Open `document` in its own sandbox, draw the whole picture at the
    /// size the drawing declares, and measure what came back.
    ///
    /// The sequence is the viewer's own — upload, open, select, render,
    /// close — so what runs here is the path a user opening a picture takes,
    /// not a shortcut around it.
    fn render(document: &str, fonts: &mut dyn FontProvider) -> Result<Rendered, RenderFailure> {
        let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), LogSink);
        send_document(&mut sandbox, document.as_bytes())
            .map_err(|_| RenderFailure::Failed("svgtext: the drawing could not be uploaded"))?;
        let opened = open_view(&mut sandbox, None, fonts).map_err(view_failure)?;
        select_page(&mut sandbox, 0).map_err(view_failure)?;
        let whole = Region {
            x: 0,
            y: 0,
            width: opened.width,
            height: opened.height,
        };
        let count = usize::try_from(opened.width)
            .ok()
            .zip(usize::try_from(opened.height).ok())
            .and_then(|(width, height)| width.checked_mul(height))
            .ok_or(RenderFailure::Failed(
                "svgtext: the drawing is larger than memory",
            ))?;
        let bytes = count
            .checked_mul(BYTES_PER_PIXEL)
            .ok_or(RenderFailure::Failed(
                "svgtext: the drawing is larger than memory",
            ))?;
        let mut out = vec![0u8; bytes];
        render_page(&mut sandbox, (opened.width, opened.height), whole, &mut out)
            .map_err(view_failure)?;
        close_view(&mut sandbox).map_err(view_failure)?;
        Ok(Rendered {
            ink: ink(&out),
            pixels: u64::try_from(count).unwrap_or(u64::MAX),
        })
    }

    /// Classify a view failure, keeping "no glyphs" apart from every other
    /// way a render can end: the control turns on exactly that distinction.
    fn view_failure(failure: ViewFailure) -> RenderFailure {
        match failure {
            ViewFailure::FontsUnavailable => RenderFailure::FontsUnavailable,
            _ => RenderFailure::Failed("svgtext: the sandboxed viewer refused the drawing"),
        }
    }

    /// Destination pixels `rendered` left non-transparent.
    ///
    /// Alpha alone, so the count is of coverage rather than of colour: a
    /// glyph drawn in any paint inks the same pixels.
    fn ink(rendered: &[u8]) -> u64 {
        let covered = rendered
            .chunks_exact(BYTES_PER_PIXEL)
            .filter(|pixel| pixel.get(3).is_some_and(|alpha| *alpha != 0))
            .count();
        u64::try_from(covered).unwrap_or(u64::MAX)
    }

    /// Render everything the measurement needs, or say what stopped it.
    fn measure() -> Result<Report, &'static str> {
        let mut fonts = ServiceFonts::new();
        let wide = render(tairix_test_svgtext::WIDE_DOCUMENT, &mut fonts).map_err(|failure| {
            failed_reason(
                failure,
                "svgtext: the wide drawing needed glyphs nothing supplied",
            )
        })?;
        let narrow =
            render(tairix_test_svgtext::NARROW_DOCUMENT, &mut fonts).map_err(|failure| {
                failed_reason(
                    failure,
                    "svgtext: the narrow drawing needed glyphs nothing supplied",
                )
            })?;
        if wide.pixels != narrow.pixels {
            return Err("svgtext: the two drawings rendered at different sizes");
        }
        // The same drawing with no seam to answer the worker's report. A
        // picture here would mean the lettering never depended on the
        // service.
        let refused_without_fonts = matches!(
            render(tairix_test_svgtext::WIDE_DOCUMENT, &mut NoFonts),
            Err(RenderFailure::FontsUnavailable)
        );
        Ok(Report {
            wide_ink: wide.ink,
            narrow_ink: narrow.ink,
            pixels: wide.pixels,
            refused_without_fonts,
        })
    }

    /// The reason a failed render reports, `unavailable` when the font seam
    /// could not furnish a face it was asked for.
    const fn failed_reason(failure: RenderFailure, unavailable: &'static str) -> &'static str {
        match failure {
            RenderFailure::FontsUnavailable => unavailable,
            RenderFailure::Failed(reason) => reason,
        }
    }

    /// Report `reason` on `stderr` and on the system log, then exit
    /// non-zero. The vertical's guest gate watches the log; a developer
    /// running the command by hand reads `stderr`.
    fn fail(sink: &LogSink, reason: &'static str) -> i32 {
        write_stderr_line(reason);
        log(
            sink,
            &Event {
                level: Level::Error,
                id: REPORT_FAILED_EVENT,
                message: REPORT_FAILED_MESSAGE,
                fields: &[Field {
                    key: "reason",
                    value: FieldValue::Str(reason),
                }],
            },
        );
        REPORT_FAILED_STATUS
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // The sandbox-worker role, before anything else: a drawing is
        // untrusted input, so it is decoded by a capability-empty child this
        // same binary is re-entered as, serving over its wired standard
        // streams and nothing else.
        if worker_role() {
            let mut service = ImageRenderService::default();
            return match serve_stdio(&mut service) {
                ServeEnd::Finished | ServeEnd::Ended => 0,
                ServeEnd::Failed(_) => WORKER_FAILED,
            };
        }
        let sink = LogSink;
        let report = match measure() {
            Ok(report) => report,
            Err(reason) => return fail(&sink, reason),
        };
        let fields = report.fields();
        sink.write_event(&Report::event(&fields));
        let verdict = report.verdict();
        if verdict.held() {
            return 0;
        }
        // The gate decides the run from the record above; this is the same
        // rule stated where a developer running the command by hand sees it.
        write_stderr_line(verdict.as_str());
        REPORT_FAILED_STATUS
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
