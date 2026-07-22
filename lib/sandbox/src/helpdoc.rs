//! The sandboxed help-document parse-and-render service.
//!
//! A help document read from a *foreign* bundle's `Help/` tree is untrusted
//! input, so its parse must not run in the calling program's process. This
//! module is the parser-sandbox service for it: the worker side parses the
//! raw bytes with `tairix_help::HelpDoc::parse` and renders the page
//! (`render_short` / `render_full`), replying with the vt-encoded render;
//! the parent side ([`render_help`]) refuses to believe the reply blindly —
//! it re-parses the returned bytes through the `lib/vt` streaming parser
//! and admits only the closed op set a help render can legitimately
//! contain (printable text, line feeds, and the bold/underline SGR pairs),
//! failing closed on anything else. A crashed or misbehaving worker is
//! contained and replaced by the [`crate::host::ParserSandbox`] seam.
//!
//! `man` is the consumer: it locates and reads the document with its own
//! file authority (`tairix_help::load_raw`), hands the bytes here, and
//! writes the validated render to standard output.

use alloc::vec::Vec;

use tairix_help::{
    render_full, render_short, HelpDoc, HelpError, Locale, RenderCtx, SectionKind, MAX_DOC_LEN,
    MAX_LOCALE_LEN,
};
use tairix_vt::{encode_all_into, Op, Parser, Sgr};

/// The render styling level, re-exported so a caller of [`render_help`] names
/// one type from this crate's surface (`man` picks it from the terminal's
/// attested colour capability).
pub use tairix_help::Styling;

use crate::host::{Launcher, ParserSandbox, SandboxError};
use crate::wire::{Reader, Writer};
use crate::worker::Service;

/// The reply cap for a rendered page. Every render of a document within
/// the `tairix_help` document bounds encodes far smaller than this; a
/// reply that exceeds it cannot be believed.
pub const MAX_RENDER: usize = 1 << 20;

/// Which render the worker runs over the parsed document.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RenderMode {
    /// The `-h` surface: NAME, SYNOPSIS, and OPTIONS, compactly.
    Short,
    /// The full `man` page: every section.
    Full,
}

/// Styling wire codes for the [`Styling`] the caller asks the render at.
const STYLE_PLAIN: u8 = 0;
const STYLE_MONOCHROME: u8 = 1;
const STYLE_COLOUR: u8 = 2;

/// A `Styling`'s wire code.
fn styling_to_wire(styling: Styling) -> u8 {
    match styling {
        Styling::Plain => STYLE_PLAIN,
        Styling::Monochrome => STYLE_MONOCHROME,
        Styling::Colour => STYLE_COLOUR,
    }
}

/// Decode a `Styling` wire code; `None` fails the request closed.
fn styling_from_wire(raw: u8) -> Option<Styling> {
    match raw {
        STYLE_PLAIN => Some(Styling::Plain),
        STYLE_MONOCHROME => Some(Styling::Monochrome),
        STYLE_COLOUR => Some(Styling::Colour),
        _ => None,
    }
}

/// Request opcode.
const OP_RENDER: u8 = 1;

/// Reply tags.
const REPLY_ERROR: u8 = 0;
const REPLY_RENDER: u8 = 1;

/// Mode wire codes.
const MODE_SHORT: u8 = 0;
const MODE_FULL: u8 = 1;

impl RenderMode {
    const fn to_wire(self) -> u8 {
        match self {
            Self::Short => MODE_SHORT,
            Self::Full => MODE_FULL,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            MODE_SHORT => Some(Self::Short),
            MODE_FULL => Some(Self::Full),
            _ => None,
        }
    }
}

/// Why the service refused a request, carried typed over the wire.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HelpRefusal {
    /// The request payload violated the request grammar.
    MalformedRequest,
    /// The document failed the fail-closed `tairix_help` parse; the
    /// carried reason is the parser's own, so the caller's diagnostic
    /// keeps full fidelity across the process boundary.
    Document(HelpError),
}

/// Typed failure [`render_help`] can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HelpRenderFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// The worker refused the request with the carried typed reason.
    Refused(HelpRefusal),
    /// The worker's reply violated the reply grammar or the render op
    /// whitelist: it cannot be believed, so the caller gets nothing
    /// (fail closed).
    ReplyMalformed,
}

/// Refusal wire codes.
const REFUSAL_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_DOCUMENT: u8 = 2;

/// `HelpError` variant wire codes. The two section-carrying variants are
/// followed on the wire by the section's index in `SectionKind::ALL`.
const DOC_TOO_LARGE: u8 = 1;
const DOC_INVALID_UTF8: u8 = 2;
const DOC_LINE_TOO_LONG: u8 = 3;
const DOC_TOO_MANY_LINES: u8 = 4;
const DOC_CONTROL_CHARACTER: u8 = 5;
const DOC_CONTENT_BEFORE_FIRST_SECTION: u8 = 6;
const DOC_UNKNOWN_HEADING: u8 = 7;
const DOC_DUPLICATE_SECTION: u8 = 8;
const DOC_SECTION_OUT_OF_ORDER: u8 = 9;
const DOC_MISSING_SECTION: u8 = 10;
const DOC_EMPTY_SECTION: u8 = 11;
const DOC_UNTERMINATED_FENCE: u8 = 12;
const DOC_MALFORMED_TABLE: u8 = 13;
const DOC_TOO_MANY_BLOCKS: u8 = 14;
const DOC_TOO_MANY_ITEMS: u8 = 15;
const DOC_TABLE_TOO_LARGE: u8 = 16;
const DOC_ORPHAN_CONTINUATION: u8 = 17;

/// Encode a `HelpError` as its wire code plus, for the section-carrying
/// variants, the section's canonical index.
fn encode_help_error(w: &mut Writer, err: HelpError) {
    let (code, section) = match err {
        HelpError::TooLarge => (DOC_TOO_LARGE, None),
        HelpError::InvalidUtf8 => (DOC_INVALID_UTF8, None),
        HelpError::LineTooLong => (DOC_LINE_TOO_LONG, None),
        HelpError::TooManyLines => (DOC_TOO_MANY_LINES, None),
        HelpError::ControlCharacter => (DOC_CONTROL_CHARACTER, None),
        HelpError::ContentBeforeFirstSection => (DOC_CONTENT_BEFORE_FIRST_SECTION, None),
        HelpError::UnknownHeading => (DOC_UNKNOWN_HEADING, None),
        HelpError::DuplicateSection => (DOC_DUPLICATE_SECTION, None),
        HelpError::SectionOutOfOrder => (DOC_SECTION_OUT_OF_ORDER, None),
        HelpError::MissingSection(kind) => (DOC_MISSING_SECTION, Some(kind)),
        HelpError::EmptySection(kind) => (DOC_EMPTY_SECTION, Some(kind)),
        HelpError::UnterminatedFence => (DOC_UNTERMINATED_FENCE, None),
        HelpError::MalformedTable => (DOC_MALFORMED_TABLE, None),
        HelpError::TooManyBlocks => (DOC_TOO_MANY_BLOCKS, None),
        HelpError::TooManyItems => (DOC_TOO_MANY_ITEMS, None),
        HelpError::TableTooLarge => (DOC_TABLE_TOO_LARGE, None),
        HelpError::OrphanContinuation => (DOC_ORPHAN_CONTINUATION, None),
    };
    w.u8(code);
    if let Some(kind) = section {
        w.u8(section_to_wire(kind));
    }
}

/// Decode the `HelpError` wire form. `None` means the bytes violate the
/// grammar and the reply cannot be believed.
fn decode_help_error(r: &mut Reader<'_>) -> Option<HelpError> {
    let code = r.u8().ok()?;
    Some(match code {
        DOC_TOO_LARGE => HelpError::TooLarge,
        DOC_INVALID_UTF8 => HelpError::InvalidUtf8,
        DOC_LINE_TOO_LONG => HelpError::LineTooLong,
        DOC_TOO_MANY_LINES => HelpError::TooManyLines,
        DOC_CONTROL_CHARACTER => HelpError::ControlCharacter,
        DOC_CONTENT_BEFORE_FIRST_SECTION => HelpError::ContentBeforeFirstSection,
        DOC_UNKNOWN_HEADING => HelpError::UnknownHeading,
        DOC_DUPLICATE_SECTION => HelpError::DuplicateSection,
        DOC_SECTION_OUT_OF_ORDER => HelpError::SectionOutOfOrder,
        DOC_MISSING_SECTION => HelpError::MissingSection(section_from_wire(r.u8().ok()?)?),
        DOC_EMPTY_SECTION => HelpError::EmptySection(section_from_wire(r.u8().ok()?)?),
        DOC_UNTERMINATED_FENCE => HelpError::UnterminatedFence,
        DOC_MALFORMED_TABLE => HelpError::MalformedTable,
        DOC_TOO_MANY_BLOCKS => HelpError::TooManyBlocks,
        DOC_TOO_MANY_ITEMS => HelpError::TooManyItems,
        DOC_TABLE_TOO_LARGE => HelpError::TableTooLarge,
        DOC_ORPHAN_CONTINUATION => HelpError::OrphanContinuation,
        _ => return None,
    })
}

/// A section kind's wire form: its index in the canonical order.
fn section_to_wire(kind: SectionKind) -> u8 {
    // The canonical array is 8 entries; the position always fits a byte.
    let position = SectionKind::ALL
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_default();
    u8::try_from(position).unwrap_or_default()
}

/// Decode a section kind from its canonical index; `None` fails closed.
fn section_from_wire(raw: u8) -> Option<SectionKind> {
    SectionKind::ALL.get(usize::from(raw)).copied()
}

/// The service the sandboxed worker runs: parse the document, render the
/// requested surface, reply with the vt-encoded ops. Total by
/// construction — every failure is a typed error reply.
#[derive(Debug, Default)]
pub struct HelpService;

impl Service for HelpService {
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        match dispatch(request) {
            Ok(reply) => reply,
            Err(refusal) => {
                let mut w = Writer::new();
                w.u8(REPLY_ERROR);
                match refusal {
                    HelpRefusal::MalformedRequest => w.u8(REFUSAL_MALFORMED_REQUEST),
                    HelpRefusal::Document(err) => {
                        w.u8(REFUSAL_DOCUMENT);
                        encode_help_error(&mut w, err);
                    }
                }
                w.finish()
            }
        }
    }
}

/// Decode the request, parse and render, and encode the reply.
fn dispatch(request: &[u8]) -> Result<Vec<u8>, HelpRefusal> {
    let mut r = Reader::new(request);
    let op = r.u8().map_err(|_| HelpRefusal::MalformedRequest)?;
    if op != OP_RENDER {
        return Err(HelpRefusal::MalformedRequest);
    }
    let mode = RenderMode::from_wire(r.u8().map_err(|_| HelpRefusal::MalformedRequest)?)
        .ok_or(HelpRefusal::MalformedRequest)?;
    let styling = styling_from_wire(r.u8().map_err(|_| HelpRefusal::MalformedRequest)?)
        .ok_or(HelpRefusal::MalformedRequest)?;
    // The served-locale tag decides the displayed heading language. It is a
    // spelling `lib/help` validates; a missing or malformed tag degrades to
    // the canonical locale (English headings) rather than refusing the page.
    let locale = r
        .string(MAX_LOCALE_LEN)
        .map_err(|_| HelpRefusal::MalformedRequest)?;
    let bytes = r
        .bytes(MAX_DOC_LEN)
        .map_err(|_| HelpRefusal::MalformedRequest)?;
    if !r.is_exhausted() {
        return Err(HelpRefusal::MalformedRequest);
    }
    let doc = HelpDoc::parse(bytes).map_err(HelpRefusal::Document)?;
    let locale = Locale::parse(&locale).unwrap_or_default();
    let ctx = RenderCtx::new(&locale, styling);
    let ops = match mode {
        RenderMode::Short => render_short(&doc, &ctx),
        RenderMode::Full => render_full(&doc, &ctx),
    };
    let mut rendered = Vec::new();
    encode_all_into(&ops, &mut rendered);
    let mut w = Writer::new();
    w.u8(REPLY_RENDER);
    w.bytes(&rendered);
    Ok(w.finish())
}

/// Ask the sandboxed worker to parse `document` and render its `mode`
/// surface at `styling`, with headings in `locale`'s language, returning
/// the validated vt-encoded render.
///
/// `locale` is the served-locale tag (e.g. `fr-FR`); the worker validates
/// it and degrades a missing or malformed tag to the canonical locale, so
/// headings simply display in English rather than the page failing.
///
/// The reply is never trusted as-is: the returned bytes are re-parsed
/// through the `lib/vt` streaming parser and every op is checked against
/// the closed set a help render can contain — printable text, line feeds,
/// and the standard colour scheme's emphasis and colour SGRs. The
/// validated ops are re-encoded canonically, so the caller writes bytes
/// this process produced, not bytes the worker chose.
///
/// # Errors
///
/// [`HelpRenderFailure`]: the sandbox failed, the worker refused the
/// request (malformed request or a typed document-parse error), or the
/// reply could not be believed.
pub fn render_help<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    mode: RenderMode,
    styling: Styling,
    locale: &str,
    document: &[u8],
) -> Result<Vec<u8>, HelpRenderFailure> {
    if document.len() > MAX_DOC_LEN {
        return Err(HelpRenderFailure::Refused(HelpRefusal::Document(
            HelpError::TooLarge,
        )));
    }
    let mut w = Writer::new();
    w.u8(OP_RENDER);
    w.u8(mode.to_wire());
    w.u8(styling_to_wire(styling));
    w.str(locale);
    w.bytes(document);
    let reply = sandbox
        .request(&w.finish())
        .map_err(HelpRenderFailure::Sandbox)?;
    decode_render_reply(&reply)
}

/// Decode and validate the worker's reply.
fn decode_render_reply(reply: &[u8]) -> Result<Vec<u8>, HelpRenderFailure> {
    let mut r = Reader::new(reply);
    let tag = r.u8().map_err(|_| HelpRenderFailure::ReplyMalformed)?;
    match tag {
        REPLY_RENDER => {
            let rendered = r
                .bytes(MAX_RENDER)
                .map_err(|_| HelpRenderFailure::ReplyMalformed)?;
            if !r.is_exhausted() {
                return Err(HelpRenderFailure::ReplyMalformed);
            }
            validate_render(rendered)
        }
        REPLY_ERROR => {
            let code = r.u8().map_err(|_| HelpRenderFailure::ReplyMalformed)?;
            let refusal = match code {
                REFUSAL_MALFORMED_REQUEST => HelpRefusal::MalformedRequest,
                REFUSAL_DOCUMENT => HelpRefusal::Document(
                    decode_help_error(&mut r).ok_or(HelpRenderFailure::ReplyMalformed)?,
                ),
                _ => return Err(HelpRenderFailure::ReplyMalformed),
            };
            if !r.is_exhausted() {
                return Err(HelpRenderFailure::ReplyMalformed);
            }
            Err(HelpRenderFailure::Refused(refusal))
        }
        _ => Err(HelpRenderFailure::ReplyMalformed),
    }
}

/// Re-parse the rendered bytes and admit only the ops a help render can
/// legitimately contain, re-encoding them canonically. Anything else —
/// cursor movement, screen clears, OSC/DCS strings, colours, a truncated
/// trailing escape — fails the whole reply closed.
fn validate_render(rendered: &[u8]) -> Result<Vec<u8>, HelpRenderFailure> {
    let mut parser = Parser::new();
    let mut ops = Vec::new();
    let mut clean = true;
    parser.feed(rendered, |op| {
        if is_render_op(&op) {
            ops.push(op);
        } else {
            clean = false;
        }
    });
    if !clean || !parser.is_ground() {
        return Err(HelpRenderFailure::ReplyMalformed);
    }
    let mut out = Vec::new();
    encode_all_into(&ops, &mut out);
    Ok(out)
}

/// The closed op whitelist of `tairix_help::render_short`/`render_full`:
/// printable text, line feeds, and the standard colour scheme's rendition
/// operations — the emphasis attributes each styled run opens (bold, dim,
/// italic, underline) and its foreground colour, plus the single reset that
/// closes it. Anything else — cursor movement, screen clears, OSC/DCS
/// strings, background colour, a truncated escape — is not something a help
/// render emits and fails the reply closed.
fn is_render_op(op: &Op) -> bool {
    matches!(
        op,
        Op::Print(_)
            | Op::LineFeed
            | Op::Sgr(
                Sgr::Reset
                    | Sgr::Bold
                    | Sgr::Dim
                    | Sgr::Italic
                    | Sgr::Underline
                    | Sgr::Foreground(_)
            )
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use tairix_help::{render_full, render_short, HelpDoc, HelpError, Locale, RenderCtx, Styling};
    use tairix_vt::encode_all_into;

    use super::{render_help, HelpRefusal, HelpRenderFailure, HelpService, RenderMode};
    use crate::host::ParserSandbox;
    use crate::loopback::LoopbackLauncher;
    use crate::worker::Service;
    use tairix_log::{Event, Sink};

    /// Discards every event (the happy paths log nothing).
    struct NullSink;

    impl Sink for NullSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    /// A minimal valid document.
    const MINIMAL: &str = "## NAME\n\ntop — display tasks\n\n## SYNOPSIS\n\n`top [-d seconds]`\n\n## DESCRIPTION\n\nShows tasks.\n";

    fn sandbox() -> ParserSandbox<LoopbackLauncher<fn() -> HelpService>, NullSink> {
        ParserSandbox::new(LoopbackLauncher::new(HelpService::default as _), NullSink)
    }

    /// The locally-computed expected render for `mode` at `styling` in
    /// `locale`.
    fn local_render(mode: RenderMode, styling: Styling, locale: &str, doc: &str) -> Vec<u8> {
        let parsed = HelpDoc::parse(doc.as_bytes()).expect("valid document");
        let locale = Locale::parse(locale).unwrap_or_default();
        let ctx = RenderCtx::new(&locale, styling);
        let ops = match mode {
            RenderMode::Short => render_short(&parsed, &ctx),
            RenderMode::Full => render_full(&parsed, &ctx),
        };
        let mut out = Vec::new();
        encode_all_into(&ops, &mut out);
        out
    }

    #[test]
    fn renders_both_surfaces_identically_to_a_local_render() {
        let mut sandbox = sandbox();
        // Full colour exercises the SGR whitelist round-trip end to end.
        for mode in [RenderMode::Short, RenderMode::Full] {
            let rendered = render_help(
                &mut sandbox,
                mode,
                Styling::Colour,
                "en-US",
                MINIMAL.as_bytes(),
            )
            .expect("renders");
            assert_eq!(
                rendered,
                local_render(mode, Styling::Colour, "en-US", MINIMAL),
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn plain_styling_carries_no_escape_and_colour_styling_does() {
        let mut sandbox = sandbox();
        let plain = render_help(
            &mut sandbox,
            RenderMode::Full,
            Styling::Plain,
            "en-US",
            MINIMAL.as_bytes(),
        )
        .expect("renders");
        assert!(!plain.contains(&0x1b), "plain output has no escapes");
        let colour = render_help(
            &mut sandbox,
            RenderMode::Full,
            Styling::Colour,
            "en-US",
            MINIMAL.as_bytes(),
        )
        .expect("renders");
        assert!(colour.contains(&0x1b), "colour output carries escapes");
    }

    #[test]
    fn the_served_locale_localises_the_headings() {
        let mut sandbox = sandbox();
        let rendered = render_help(
            &mut sandbox,
            RenderMode::Full,
            Styling::Plain,
            "fr-FR",
            MINIMAL.as_bytes(),
        )
        .expect("renders");
        let text = alloc::string::String::from_utf8(rendered).expect("utf-8");
        assert!(text.contains("NOM"), "French NAME heading: {text}");
        assert!(
            text.contains("DESCRIPTION"),
            "French DESCRIPTION heading: {text}"
        );
        assert!(
            !text.contains("NAME"),
            "English NAME must not appear: {text}"
        );
    }

    #[test]
    fn a_malformed_document_round_trips_its_typed_error() {
        let mut sandbox = sandbox();
        let direct = HelpDoc::parse(b"garbage before any heading\n").unwrap_err();
        assert_eq!(
            render_help(
                &mut sandbox,
                RenderMode::Full,
                Styling::Plain,
                "en-US",
                b"garbage before any heading\n"
            ),
            Err(HelpRenderFailure::Refused(HelpRefusal::Document(direct)))
        );
    }

    #[test]
    fn a_section_carrying_error_round_trips_intact() {
        let mut sandbox = sandbox();
        let doc = b"## NAME\n\nx \xe2\x80\x94 y\n";
        let direct = HelpDoc::parse(doc).unwrap_err();
        assert!(matches!(direct, HelpError::MissingSection(_)));
        assert_eq!(
            render_help(&mut sandbox, RenderMode::Full, Styling::Plain, "en-US", doc),
            Err(HelpRenderFailure::Refused(HelpRefusal::Document(direct)))
        );
    }

    #[test]
    fn an_oversize_document_is_refused_before_any_send() {
        let mut sandbox = sandbox();
        let oversize = vec![b'a'; tairix_help::MAX_DOC_LEN + 1];
        assert_eq!(
            render_help(
                &mut sandbox,
                RenderMode::Full,
                Styling::Plain,
                "en-US",
                &oversize
            ),
            Err(HelpRenderFailure::Refused(HelpRefusal::Document(
                HelpError::TooLarge
            )))
        );
    }

    #[test]
    fn the_service_refuses_a_malformed_request() {
        let mut service = HelpService;
        // Unknown opcode.
        assert_eq!(service.handle(&[0xff]), vec![super::REPLY_ERROR, 1]);
        // Truncated request.
        assert_eq!(service.handle(&[]), vec![super::REPLY_ERROR, 1]);
        // Unknown mode.
        assert_eq!(
            service.handle(&[super::OP_RENDER, 9]),
            vec![super::REPLY_ERROR, 1]
        );
    }

    /// A hostile worker: replies `REPLY_RENDER` carrying the given bytes,
    /// exactly as a compromised parser process could.
    struct EvilWorker(Vec<u8>);

    impl Service for EvilWorker {
        fn handle(&mut self, _request: &[u8]) -> Vec<u8> {
            let mut w = crate::wire::Writer::new();
            w.u8(super::REPLY_RENDER);
            w.bytes(&self.0);
            w.finish()
        }
    }

    fn evil_sandbox(
        reply: &'static [u8],
    ) -> ParserSandbox<LoopbackLauncher<impl FnMut() -> EvilWorker>, NullSink> {
        ParserSandbox::new(
            LoopbackLauncher::new(move || EvilWorker(reply.to_vec())),
            NullSink,
        )
    }

    #[test]
    fn a_forbidden_escape_in_the_reply_fails_closed() {
        // A screen clear, an OSC title change, a background colour, and
        // reverse video: none is an operation a help render emits, so each
        // must fail the reply closed however plausible the surrounding text.
        // (A *foreground* colour, by contrast, is now a legitimate render op.)
        for evil in [
            b"safe\x1b[2Jtext".as_slice(),
            b"safe\x1b]0;owned\x07text".as_slice(),
            b"safe\x1b[41mtext".as_slice(),
            b"safe\x1b[7mtext".as_slice(),
        ] {
            let mut sandbox = evil_sandbox(evil);
            assert_eq!(
                render_help(
                    &mut sandbox,
                    RenderMode::Full,
                    Styling::Colour,
                    "en-US",
                    MINIMAL.as_bytes()
                ),
                Err(HelpRenderFailure::ReplyMalformed),
                "reply {evil:?}"
            );
        }
    }

    #[test]
    fn a_truncated_trailing_escape_fails_closed() {
        let mut sandbox = evil_sandbox(b"text\x1b[");
        assert_eq!(
            render_help(
                &mut sandbox,
                RenderMode::Full,
                Styling::Colour,
                "en-US",
                MINIMAL.as_bytes()
            ),
            Err(HelpRenderFailure::ReplyMalformed)
        );
    }

    #[test]
    fn a_clean_styled_reply_is_re_encoded_canonically() {
        // A coloured, bold heading closed by a single reset — exactly the
        // shape a genuine render emits, in the canonical one-CSI-per-op form
        // the emitter produces: bold (SGR 1), bright-blue foreground (SGR 94),
        // the text, then reset (SGR 0).
        let mut sandbox = evil_sandbox(b"\x1b[1m\x1b[94mNAME\x1b[0m\nbody");
        let out = render_help(
            &mut sandbox,
            RenderMode::Full,
            Styling::Colour,
            "en-US",
            MINIMAL.as_bytes(),
        )
        .expect("whitelisted ops pass");
        assert_eq!(out, b"\x1b[1m\x1b[94mNAME\x1b[0m\nbody");
    }
}
