//! The one geometry the authentication surface paints, hit-tests, and reports
//! damage against.
//!
//! The screen is a single centred column: the clock, date, and host name at
//! the top, and beneath them either the account tiles or the chosen account's
//! disc, name, and secret field. Both bodies hang off the same chrome block
//! and are centred in the space under it, so choosing an account does not
//! make the screen jump.
//!
//! Every length is authored in *logical* pixels at the reference density and
//! converted through the one shared [`Scale`], so the composition is the same
//! at any DPI. Each band is a fixed logical height with its text centred
//! inside it, which is what lets [`crate::panel_rect`] answer where the
//! prompt is without measuring a font.

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::Rgba;

/// Gap from the top of the screen to the chrome block.
const CHROME_TOP: u32 = 40;

/// The clock's band: the dominant line on the screen.
pub(crate) const CLOCK_BAND: u32 = 64;

/// The date's band, under the clock.
pub(crate) const DATE_BAND: u32 = 24;

/// The host name's band, under the date.
pub(crate) const HOST_BAND: u32 = 18;

/// The whole chrome block's height.
const CHROME_HEIGHT: u32 = CLOCK_BAND + DATE_BAND + HOST_BAND;

/// Gap between the chrome block and the body under it.
const BODY_GAP: u32 = 28;

/// The smallest body the chrome will give up room for.
///
/// The prompt body, because asking for a secret is what the screen is *for*:
/// a screen that cannot hold the clock and still show the prompt keeps the
/// prompt.
const MIN_BODY: u32 = PROMPT_BODY;

/// The chosen account's disc, on the prompt.
pub(crate) const AVATAR_SIDE: u32 = 88;

/// Gap between that disc and the account name under it.
pub(crate) const AVATAR_GAP: u32 = 14;

/// The account name's band.
pub(crate) const NAME_BAND: u32 = 26;

/// Gap between the account name and the prompt block under it.
pub(crate) const NAME_GAP: u32 = 18;

/// The prompt block's width.
///
/// Wider than the field, because the notice under it is prose and a block
/// only as wide as the pill would cut it short.
const PROMPT_WIDTH: u32 = 420;

/// The secret field's width, centred in the block.
pub(crate) const FIELD_WIDTH: u32 = 320;

/// Gap between the secret field and the notice under it.
const NOTICE_GAP: u32 = 10;

/// The notice line's band.
pub(crate) const NOTICE_BAND: u32 = 20;

/// Gap between the notice and the step-back line.
const BACK_GAP: u32 = 6;

/// The step-back line's band.
const BACK_BAND: u32 = 18;

/// The prompt block's height: the secret field, the notice, and the
/// step-back line.
///
/// The field's own row height comes from the theme's control metric, so the
/// block reserves comfortably more than the shipped one needs and every line
/// under the field is placed against the field's *actual* rectangle and
/// dropped if the block runs out — the block therefore always contains
/// everything drawn in it, whatever the theme.
pub(crate) const PROMPT_HEIGHT: u32 = 96;

/// The prompt body's height: the disc, the account name, and the block.
pub(crate) const PROMPT_BODY: u32 = AVATAR_SIDE + AVATAR_GAP + NAME_BAND + NAME_GAP + PROMPT_HEIGHT;

/// Gap between the tile grid and the chooser's one hint line.
pub(crate) const CHOOSER_HINT_GAP: u32 = 20;

/// Margin kept clear at each side of the screen, so a wide row of tiles
/// never runs to the very edge.
pub(crate) const SIDE_MARGIN: u32 = 32;

/// Where the prompt's three parts sit on the screen.
///
/// One definition, so the disc, the name, and the block cannot drift apart —
/// and so [`crate::panel_rect`], which the embedder asks for the region whose
/// legibility the scrim must protect, is the very block the field is drawn
/// in rather than a second guess at it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Prompt {
    /// The chosen account's disc, centred at the top of the body.
    pub(crate) disc: Rect,
    /// The full-width band the account's name is centred in, so a long name
    /// is not cut to the block's width.
    pub(crate) name: Rect,
    /// The block holding the field and the lines under it.
    pub(crate) block: Rect,
}

impl Prompt {
    /// The prompt's geometry on `screen`.
    pub(crate) fn new(screen: Rect, scale: Scale) -> Self {
        let body_top = Column::new(screen, scale, scale.scale_length(PROMPT_BODY)).body_top;
        let side = scale.scale_length(AVATAR_SIDE).min(screen.width);
        let disc = Rect::new(
            centre_on(screen.origin.x, screen.width, side),
            body_top,
            side,
            side,
        );
        let name = Rect::new(
            screen.origin.x,
            down(down(disc.origin.y, side), scale.scale_length(AVATAR_GAP)),
            screen.width,
            scale.scale_length(NAME_BAND),
        );
        let width = scale.scale_length(PROMPT_WIDTH).min(screen.width);
        let block = Rect::new(
            centre_on(screen.origin.x, screen.width, width),
            down(
                down(name.origin.y, name.height),
                scale.scale_length(NAME_GAP),
            ),
            width,
            scale.scale_length(PROMPT_HEIGHT).min(screen.height),
        );
        Self { disc, name, block }
    }
}

/// Where the chrome block sits and where the body under it starts.
///
/// One definition for both modes: the chooser and the prompt pass their own
/// body height and get the same chrome, so the top of the screen is
/// identical either side of choosing an account.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Column {
    /// The full-width band the clock, date, and host name are drawn in, or
    /// [`Rect::EMPTY`] when the screen has no room for it.
    pub(crate) chrome: Rect,
    /// The top of the body, in the surface's own coordinates.
    pub(crate) body_top: i32,
}

impl Column {
    /// The column a `body_height`-tall body makes on `screen`.
    ///
    /// The body is centred in the space under the chrome, and starts at the
    /// top of that space when it is taller than the space itself, so it can
    /// never ride up over the clock.
    pub(crate) fn new(screen: Rect, scale: Scale, body_height: u32) -> Self {
        let chrome = chrome_band(screen, scale);
        let region_top = if chrome.height == 0 {
            screen.origin.y
        } else {
            down(chrome.bottom(), scale.scale_length(BODY_GAP))
        };
        let slack = i64::from(down(screen.origin.y, screen.height))
            - i64::from(region_top)
            - i64::from(body_height);
        Self {
            chrome,
            body_top: down(region_top, halve(slack)),
        }
    }
}

/// The band the clock, date, and host name are drawn in on `screen`, or
/// [`Rect::EMPTY`] when the screen has no room for it.
///
/// A function of the screen and the density alone — never of which body is
/// up — so the top of the screen is identical either side of choosing an
/// account. A chooser whose grid is taller than a prompt must not be the
/// reason the clock disappears.
pub(crate) fn chrome_band(screen: Rect, scale: Scale) -> Rect {
    let height = scale.scale_length(CHROME_HEIGHT);
    let needed = scale
        .scale_length(CHROME_TOP)
        .saturating_add(height)
        .saturating_add(scale.scale_length(BODY_GAP))
        .saturating_add(scale.scale_length(MIN_BODY));
    if screen.height < needed {
        return Rect::EMPTY;
    }
    Rect::new(
        screen.origin.x,
        down(screen.origin.y, scale.scale_length(CHROME_TOP)),
        screen.width,
        height,
    )
}

/// The three bands of the chrome block, in the order they are drawn.
///
/// Returned as rectangles rather than as heights so the caller centres each
/// line in its own band instead of stepping a pen down by font metrics — the
/// composition then holds whatever the theme sizes its text at.
pub(crate) fn chrome_bands(chrome: Rect, scale: Scale) -> [Rect; 3] {
    let mut top = chrome.origin.y;
    [CLOCK_BAND, DATE_BAND, HOST_BAND].map(|band| {
        let height = scale.scale_length(band);
        let rect = Rect::new(chrome.origin.x, top, chrome.width, height);
        top = down(top, height);
        rect
    })
}

/// `extent` centred within `[origin, origin + available)`.
pub(crate) fn centre_on(origin: i32, available: u32, extent: u32) -> i32 {
    down(origin, available.saturating_sub(extent) / 2)
}

/// `origin` moved on by `offset` pixels, saturating rather than wrapping.
pub(crate) fn down(origin: i32, offset: u32) -> i32 {
    origin.saturating_add(i32::try_from(offset).unwrap_or(i32::MAX))
}

/// Half of `span`, clamped into the coordinate range and never negative.
fn halve(span: i64) -> u32 {
    u32::try_from((span / 2).max(0)).unwrap_or(u32::MAX)
}

/// The band the notice sits in: under `field`, across the prompt `block`.
///
/// `None` when the block has no room left for it, which is what keeps the
/// block containing everything drawn in it however tall the theme's own
/// control row turns out to be.
pub(crate) fn notice_band(block: Rect, field: Rect, scale: Scale) -> Option<Rect> {
    confine(
        Rect::new(
            block.origin.x,
            down(
                down(field.origin.y, field.height),
                scale.scale_length(NOTICE_GAP),
            ),
            block.width,
            scale.scale_length(NOTICE_BAND),
        ),
        block,
    )
}

/// The band the step-back line sits in, under the `notice` band.
pub(crate) fn back_band(block: Rect, notice: Rect, scale: Scale) -> Option<Rect> {
    confine(
        Rect::new(
            block.origin.x,
            down(
                down(notice.origin.y, notice.height),
                scale.scale_length(BACK_GAP),
            ),
            block.width,
            scale.scale_length(BACK_BAND),
        ),
        block,
    )
}

/// `band` itself when it fits inside `bounds`, or `None` when it does not.
fn confine(band: Rect, bounds: Rect) -> Option<Rect> {
    let top = band.origin.y;
    let bottom = down(bounds.origin.y, bounds.height);
    if band.height == 0 || top < bounds.origin.y || down(top, band.height) > bottom {
        return None;
    }
    Some(band)
}

/// Draw `text` centred in `band`, in `ink`, truncated to the band's width.
///
/// The one line-drawing definition the column shares, so the clock, the
/// account name, the notice, and the step-back line all centre and clip
/// alike. A band shorter than the font's line box draws nothing rather than
/// letting text spill past the rectangle the surface reports as damaged.
pub(crate) fn draw_centred(
    surface: &mut Surface,
    band: Rect,
    text: &str,
    font: BitmapFont,
    ink: Rgba,
) {
    let line = font.line_height();
    if text.is_empty() || band.width == 0 || line > band.height {
        return;
    }
    let shown = font.truncate_to_width(text, band.width);
    let width = font.text_width(shown);
    font.draw_text(
        surface,
        centre_on(band.origin.x, band.width, width),
        down(band.origin.y, (band.height - line) / 2),
        shown,
        Color::from(ink),
    );
}
