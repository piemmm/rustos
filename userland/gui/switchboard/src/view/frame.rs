//! The one section-frame anatomy every Switchboard section lays out into
//! (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Every section is the same shape:
//!
//! ```text
//!  sidebar? |            header?                                  |
//!           |  primary   |  detail?   |  impact?   |  rail?     |
//!           |            footer?                                  |
//! ```
//!
//! A section states what it wants, in logical (unscaled) lengths, as a
//! [`SectionAnatomy`]; [`resolve_section_frame`] is the *one* place that
//! request is turned into the physical [`SectionFrame`] rectangles every
//! section draws into and hit-tests against. No section restates this
//! geometry, and a content rect too narrow or too short to seat every region
//! resolves the same way everywhere rather than as a per-section
//! improvisation.

use tairix_geometry::{to_i32, Rect, Scale};
use tairix_theme::Theme;

/// The logical width of an anchored action rail, whichever section seats
/// one.
///
/// The rail is one control with one presentation, so its column is the same
/// width wherever it appears: a reader who learns where the commands sit in
/// one section finds them in the same place in the next. Sections state
/// this rather than each choosing a width that happens to coincide.
pub const ACTION_RAIL_WIDTH: u32 = 136;

/// The logical minimum width of a master/detail section's detail pane,
/// whichever section seats one.
///
/// Every detail pane is the same thing — the plate describing whichever
/// item the master list has selected — so it claims the same column in
/// every section: a reader who learns where the detail sits in one section
/// finds it in the same place in the next, and a window narrow enough to
/// shed the pane sheds it everywhere at once rather than section by
/// section. Wide enough at the reference density for the widest thing a
/// pane seats (a register table's name and value columns).
pub const DETAIL_PANE_WIDTH: u32 = 208;

/// What a section seats in the trailing end of the shared location band, in
/// logical (unscaled) lengths.
///
/// The band names where the reader is; a section with a *census* of its own —
/// a handful of counts describing the whole list at a glance — shows it there,
/// beside that name, rather than spending a row of its own content on it. The
/// band is chrome shared by every section, so the section states the room it
/// needs and [`resolve_band`] decides where it goes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BandSummary {
    /// The width the summary needs, trailing the band's own command.
    pub width: u32,
    /// The height the whole band must be to seat it.
    pub height: u32,
}

/// What a section asks the frame to seat, in logical (unscaled) lengths.
///
/// A zero length means the section does not want that region at all: the
/// frame gives it `None` (for the optional regions) or a zero-height row
/// (for the header/footer) rather than a sliver nobody asked for. `primary`
/// carries no field here because it is not optional — it is whatever the
/// frame has left after the regions below are seated.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SectionAnatomy {
    /// The summary this section seats in the shared location band, or `None`
    /// when it has none and the band keeps its resting height.
    pub band_summary: Option<BandSummary>,
    /// The leading navigation column's width (e.g. System's page `Tabs`).
    /// Zero when the section has no sidebar.
    pub sidebar_width: u32,
    /// The header row's height, above the primary/detail/rail row. Zero
    /// when the section has no header.
    pub header_height: u32,
    /// The detail pane's minimum width, trailing `primary`. Zero when the
    /// section has no detail pane.
    pub detail_width: u32,
    /// The impact column's width, between the detail pane and the rail.
    /// Zero when the section has no impact column.
    ///
    /// This is the narrow stack of readings a section shows *about* the
    /// subject its detail pane describes (Recovery's per-task CPU, memory,
    /// disk and network tiles), kept a region of its own rather than
    /// squeezed into `detail` so a content rect too narrow for it sheds it
    /// on its own terms.
    pub impact_width: u32,
    /// The trailing action rail's width. Zero when the section has no rail.
    pub rail_width: u32,
    /// The footer row's height, below the primary/detail/rail row. Zero
    /// when the section has no footer.
    pub footer_height: u32,
    /// How many inline commands the widest row of this section's primary
    /// column seats. Zero when its rows carry no inline commands.
    ///
    /// A row's trailing command strip is a *fixed* physical width — one
    /// action width per command, plus the gaps and the trailing inset — so
    /// it is the one part of `primary` that cannot give way when the window
    /// narrows. Stating the count here lets [`resolve_section_frame`] shed an
    /// optional column instead of squeezing the strip off the row's own edge,
    /// which is what the drop order is for.
    ///
    /// The count is stated rather than the width because the width depends on
    /// [`Theme::metrics`] and the active [`Scale`], neither of which a section
    /// has when it declares its anatomy. [`Self::primary_floor`] turns the
    /// count into that width in the single place the arithmetic lives, so a
    /// section cannot declare a floor that disagrees with the strip its rows
    /// actually draw.
    pub primary_row_commands: u32,
}

impl SectionAnatomy {
    /// The anatomy of a section with no sidebar, header, detail pane, rail
    /// or footer — just the primary column filling the whole content rect.
    pub const PRIMARY_ONLY: Self = Self {
        band_summary: None,
        sidebar_width: 0,
        header_height: 0,
        detail_width: 0,
        impact_width: 0,
        rail_width: 0,
        footer_height: 0,
        primary_row_commands: 0,
    };

    /// The physical height the location band must be for this section: its
    /// resting one control height, or as much more as its band summary needs.
    ///
    /// The band is shared chrome, so its height is asked of whichever section
    /// is on show rather than fixed: a section with no summary gets exactly
    /// the resting band every other one has.
    #[must_use]
    pub fn band_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let resting = scale.scale_length(theme.metrics().control_height).max(1);
        match self.band_summary {
            Some(summary) => resting.max(scale.scale_length(summary.height)),
            None => resting,
        }
    }

    /// The narrowest physical width `primary` may be given before
    /// [`resolve_section_frame`] sheds an optional column to widen it.
    ///
    /// A section whose rows carry no inline commands floors at one physical
    /// pixel — the width below which `primary` would not exist at all. A
    /// section whose rows do carry commands floors at exactly what that strip
    /// claims of a row: the commands themselves, the row's trailing inset
    /// outside them, and the gap that keeps them off the row's text. At the
    /// floor the row's text is squeezed to nothing but every command is still
    /// inside its own row, which is the outcome worth protecting; anything
    /// wider would be a text allowance nobody asked for, shedding a column a
    /// reader could have had.
    #[must_use]
    pub fn primary_floor(self, scale: Scale, theme: &Theme) -> u32 {
        if self.primary_row_commands == 0 {
            return 1;
        }
        let m = theme.metrics();
        scale
            .scale_length(m.control_inset)
            .max(1)
            .saturating_add(row_commands_width(self.primary_row_commands, scale, theme))
            .saturating_add(scale.scale_length(m.control_gap).max(1))
            .max(1)
    }

    /// The narrowest physical content width this anatomy needs so
    /// [`resolve_section_frame`] would not have to drop any of the optional
    /// regions it asks for to keep `primary` at its declared floor.
    ///
    /// This is the sum of the sidebar, detail, impact and rail widths this
    /// anatomy asks for — each with the one [`Theme::metrics`] control gap
    /// [`resolve_section_frame`] reserves beside a region that is actually
    /// present — plus [`Self::primary_floor`], the width `primary` itself may
    /// not fall below.
    ///
    /// This is *not* the window's minimum client width: the optional columns
    /// counted here are shed rather than clipped when they do not fit, so a
    /// window narrower than this still renders correctly with fewer columns.
    /// What a window must guarantee is [`Self::primary_floor`]; this is the
    /// width at which nothing has to be shed.
    #[must_use]
    pub fn minimum_width(self, scale: Scale, theme: &Theme) -> u32 {
        let gap = scale.scale_length(theme.metrics().control_gap);
        region_block(
            scale.scale_length(self.sidebar_width),
            gap,
            self.sidebar_width > 0,
        )
        .saturating_add(region_block(
            scale.scale_length(self.detail_width),
            gap,
            self.detail_width > 0,
        ))
        .saturating_add(region_block(
            scale.scale_length(self.impact_width),
            gap,
            self.impact_width > 0,
        ))
        .saturating_add(region_block(
            scale.scale_length(self.rail_width),
            gap,
            self.rail_width > 0,
        ))
        .saturating_add(self.primary_floor(scale, theme))
    }

    /// The shortest logical content height this anatomy needs so its header
    /// and footer are drawn at their full requested height, with one
    /// physical pixel left over for the primary/detail/rail row to stay
    /// non-empty.
    ///
    /// Unlike the optional width regions, [`resolve_section_frame`] never
    /// drops the header or footer outright — a content shorter than this
    /// simply clips them — so this is a "comfortable enough" floor rather
    /// than a threshold below which something disappears. A host derives the
    /// window's minimum client height from the widest of this across every
    /// section it composes.
    ///
    /// Unlike [`minimum_width`](Self::minimum_width) this needs no theme: the
    /// header and footer stack directly against `primary` with no gap
    /// between them (see [`resolve_section_frame`]), so only the scale
    /// applies.
    #[must_use]
    pub fn minimum_height(self, scale: Scale) -> u32 {
        scale
            .scale_length(self.header_height)
            .saturating_add(scale.scale_length(self.footer_height))
            .saturating_add(1)
    }
}

/// The physical width of a region already-scaled to `width` physical
/// pixels, plus its trailing control gap, when it is `present` — zero
/// otherwise.
///
/// Shared by [`resolve_section_frame`]'s region-fitting search and
/// [`SectionAnatomy::minimum_width`], so the "one gap per present region"
/// rule is defined once rather than restated by both. Presence is decided
/// by the caller from the *logical* length being non-zero, never from
/// whether the scaled `width` happens to round to zero, so an extreme scale
/// can never make a region that was asked for silently vanish from this
/// arithmetic.
fn region_block(width: u32, gap: u32, present: bool) -> u32 {
    if present {
        width.saturating_add(gap)
    } else {
        0
    }
}

/// The physical width reserved for one inline row-command button.
///
/// Every inline command is the same size wherever it appears, so a reader
/// who learns how much of a row its commands claim finds the same strip in
/// the next section. This is the one definition: the row splitter that lays
/// the buttons out and [`SectionAnatomy::primary_floor`] both read it, so the
/// floor a section declares cannot drift from the strip its rows draw.
#[must_use]
pub fn action_button_width(scale: Scale, theme: &Theme) -> u32 {
    scale
        .scale_length(theme.metrics().control_height.saturating_mul(4))
        .max(1)
}

/// The physical width `commands` inline row buttons occupy side by side —
/// the buttons and the gaps between them, without the row's trailing inset.
///
/// Zero for no commands, so a row with none reserves nothing.
#[must_use]
pub fn row_commands_width(commands: u32, scale: Scale, theme: &Theme) -> u32 {
    if commands == 0 {
        return 0;
    }
    let button = action_button_width(scale, theme);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    button
        .saturating_mul(commands)
        .saturating_add(gap.saturating_mul(commands.saturating_sub(1)))
}

/// The location band resolved to physical rectangles: the trail naming where
/// the reader is, the command that opens the section list, and the active
/// section's own summary.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BandLayout {
    /// Where the [`Breadcrumb`](tairix_controls::Breadcrumb) draws.
    pub trail: Rect,
    /// The square the trailing section-list command occupies.
    pub command: Rect,
    /// The section's own summary, or `None` when it asked for none or the
    /// band was too narrow to seat it.
    pub summary: Option<Rect>,
}

/// Resolve the location band into the rectangles its paint and its hit test
/// both read, so a press can never land on a control drawn somewhere else.
///
/// The band reads leading to trailing: the trail, then the active section's
/// `summary`, then the section-list command as a square at the very edge —
/// the shape an [`IconButton`](tairix_controls::IconButton) wants. The trail
/// takes whatever is left, so it is the region that gives way as the window
/// narrows, and a band with no room for the summary drops it rather than
/// starving the trail: the reader's own location outranks a census they can
/// still read from the table.
#[must_use]
pub fn resolve_band(
    location: Rect,
    summary: Option<BandSummary>,
    scale: Scale,
    theme: &Theme,
) -> BandLayout {
    let side = location.height.min(location.width);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let command = Rect::new(
        location.right() - to_i32(side),
        location.top(),
        side,
        location.height,
    );
    let after_command = location.width.saturating_sub(side).saturating_sub(gap);

    // The trail keeps at least the width its own leading crumb needs before
    // the summary is seated at all; below that there is nothing to share.
    let wanted = summary.map_or(0, |summary| scale.scale_length(summary.width));
    let floor = scale.scale_length(TRAIL_FLOOR);
    let seated = (wanted > 0 && after_command >= wanted.saturating_add(gap).saturating_add(floor))
        .then_some(wanted);
    let summary = seated.map(|width| {
        Rect::new(
            location.left() + to_i32(after_command.saturating_sub(width)),
            location.top(),
            width,
            location.height,
        )
    });

    let trail_w = match seated {
        Some(width) => after_command
            .saturating_sub(width)
            .saturating_sub(gap)
            .min(location.width),
        None => after_command.min(location.width),
    };
    let trail = Rect::new(location.left(), location.top(), trail_w, location.height);
    BandLayout {
        trail,
        command,
        summary,
    }
}

/// The logical width the location trail keeps for itself before a section's
/// band summary is seated beside it.
///
/// The trail names where the reader is, which is the band's whole purpose, so
/// it is never reduced to an ellipsis to make room for a census.
const TRAIL_FLOOR: u32 = 160;

/// The [`SectionAnatomy`] resolved to physical rectangles within one content
/// rect (`plans/NEW-SWITCHBOARD.md` S3).
///
/// `primary` is the only region guaranteed to keep its declared floor
/// whenever `content` itself has the width to give it: [`resolve_section_frame`]
/// drops the optional regions before it ever starves `primary`. `header`
/// and `footer` are always present, possibly with zero height; `sidebar`,
/// `detail`, `impact` and `rail` are `None` exactly when the section did
/// not ask for them or the content was too narrow to seat them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SectionFrame {
    /// The leading navigation column, or `None` when not asked for or not
    /// seated.
    pub sidebar: Option<Rect>,
    /// The header row. Zero height when not asked for, clipped when the
    /// content is too short.
    pub header: Rect,
    /// The master list or table. Never empty in width while `content` has
    /// any width to give it.
    pub primary: Rect,
    /// The detail pane, or `None` when not asked for or not seated.
    pub detail: Option<Rect>,
    /// The impact column between the detail pane and the rail, or `None`
    /// when not asked for or not seated.
    pub impact: Option<Rect>,
    /// The trailing action rail, or `None` when not asked for or not
    /// seated.
    pub rail: Option<Rect>,
    /// The footer row. Zero height when not asked for, clipped when the
    /// content is too short.
    pub footer: Rect,
}

/// Resolve `anatomy` against `content` for `scale`/`theme`: the one place
/// every section's logical request becomes the physical rectangles it draws
/// into and hit-tests against.
///
/// When `content` cannot seat every region `anatomy` asked for *and* leave
/// `primary` at [`SectionAnatomy::primary_floor`], the optional regions are
/// dropped in exactly this order — `detail`, then `impact`, then `rail`,
/// then `sidebar` — until the floor is met, so `primary` is the last region
/// ever starved and the drop order is a property of the frame rather than
/// something each section re-decides. Shedding on the *floor* rather than on
/// `primary` reaching zero is what keeps a row's fixed command strip inside
/// its own row: a section stating that strip gets an optional column shed
/// for it instead of commands pushed off the edge. A `content` narrower than
/// the floor itself has nothing left to shed and simply gets the whole width
/// as `primary`, which is still the most usable thing left. `impact` follows
/// `detail` out because a stack of readings about the selected subject is
/// worth less than the commands that act on it once space runs short. The
/// header and footer collapse to zero height when the section did not ask
/// for them, and are clipped (header first, footer from whatever height is
/// left) rather than driven negative when `content` is shorter than their
/// combined logical height.
///
/// Fails closed throughout: every dimension is derived with saturating
/// arithmetic, so a pathological `anatomy` or a zero-sized `content` yields
/// empty regions rather than a negative or overlapping rectangle.
#[must_use]
pub fn resolve_section_frame(
    content: Rect,
    anatomy: SectionAnatomy,
    scale: Scale,
    theme: &Theme,
) -> SectionFrame {
    let gap = scale.scale_length(theme.metrics().control_gap);
    let floor = anatomy.primary_floor(scale, theme);
    let scaled_sidebar = scale.scale_length(anatomy.sidebar_width);
    let scaled_detail = scale.scale_length(anatomy.detail_width);
    let scaled_impact = scale.scale_length(anatomy.impact_width);
    let scaled_rail = scale.scale_length(anatomy.rail_width);

    let mut sidebar_on = anatomy.sidebar_width > 0;
    let mut detail_on = anatomy.detail_width > 0;
    let mut impact_on = anatomy.impact_width > 0;
    let mut rail_on = anatomy.rail_width > 0;

    // The width a given combination of seated regions leaves for `primary`.
    let primary_width_for =
        |sidebar_on: bool, detail_on: bool, impact_on: bool, rail_on: bool| -> u32 {
            let body_w =
                content
                    .width
                    .saturating_sub(region_block(scaled_sidebar, gap, sidebar_on));
            body_w
                .saturating_sub(region_block(scaled_detail, gap, detail_on))
                .saturating_sub(region_block(scaled_impact, gap, impact_on))
                .saturating_sub(region_block(scaled_rail, gap, rail_on))
        };

    if primary_width_for(sidebar_on, detail_on, impact_on, rail_on) < floor && detail_on {
        detail_on = false;
    }
    if primary_width_for(sidebar_on, detail_on, impact_on, rail_on) < floor && impact_on {
        impact_on = false;
    }
    if primary_width_for(sidebar_on, detail_on, impact_on, rail_on) < floor && rail_on {
        rail_on = false;
    }
    if primary_width_for(sidebar_on, detail_on, impact_on, rail_on) < floor && sidebar_on {
        sidebar_on = false;
    }

    let sidebar_w = if sidebar_on { scaled_sidebar } else { 0 };
    let detail_w = if detail_on { scaled_detail } else { 0 };
    let impact_w = if impact_on { scaled_impact } else { 0 };
    let rail_w = if rail_on { scaled_rail } else { 0 };
    let primary_w = primary_width_for(sidebar_on, detail_on, impact_on, rail_on);

    let sidebar_block = region_block(scaled_sidebar, gap, sidebar_on);
    let body_left = content.left().saturating_add(to_i32(sidebar_block));
    let body_width = content.width.saturating_sub(sidebar_block);

    let sidebar =
        sidebar_on.then(|| Rect::new(content.left(), content.top(), sidebar_w, content.height));

    let header_h = scale
        .scale_length(anatomy.header_height)
        .min(content.height);
    let after_header = content.height.saturating_sub(header_h);
    let footer_h = scale.scale_length(anatomy.footer_height).min(after_header);
    let middle_h = after_header.saturating_sub(footer_h);

    let header = Rect::new(body_left, content.top(), body_width, header_h);
    let middle_top = content.top().saturating_add(to_i32(header_h));
    let footer_top = middle_top.saturating_add(to_i32(middle_h));
    let footer = Rect::new(body_left, footer_top, body_width, footer_h);

    let primary = Rect::new(body_left, middle_top, primary_w, middle_h);

    let mut cursor = primary_w;
    if detail_on {
        cursor = cursor.saturating_add(gap);
    }
    let detail_left = body_left.saturating_add(to_i32(cursor));
    if detail_on {
        cursor = cursor.saturating_add(detail_w);
    }
    if impact_on {
        cursor = cursor.saturating_add(gap);
    }
    let impact_left = body_left.saturating_add(to_i32(cursor));
    if impact_on {
        cursor = cursor.saturating_add(impact_w);
    }
    if rail_on {
        cursor = cursor.saturating_add(gap);
    }
    let rail_left = body_left.saturating_add(to_i32(cursor));

    let detail = detail_on.then(|| Rect::new(detail_left, middle_top, detail_w, middle_h));
    let impact = impact_on.then(|| Rect::new(impact_left, middle_top, impact_w, middle_h));
    let rail = rail_on.then(|| Rect::new(rail_left, middle_top, rail_w, middle_h));

    SectionFrame {
        sidebar,
        header,
        primary,
        detail,
        impact,
        rail,
        footer,
    }
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
