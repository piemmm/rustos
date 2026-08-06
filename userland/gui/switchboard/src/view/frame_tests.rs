//! Unit tests for the shared section-frame anatomy and its resolution
//! (`plans/NEW-SWITCHBOARD.md` S3).

use tairix_geometry::{Rect, Scale};
use tairix_theme::Theme;

use super::{resolve_band, resolve_section_frame, BandSummary, SectionAnatomy};

/// An anatomy asking for every optional region, each a comfortably small
/// logical size so a modest content rect can seat them all.
fn full_anatomy() -> SectionAnatomy {
    SectionAnatomy {
        band_summary: None,
        sidebar_width: 48,
        header_height: 32,
        detail_width: 96,
        impact_width: 56,
        rail_width: 64,
        footer_height: 24,
        primary_row_commands: 0,
    }
}

/// The regions of `frame` that are present, as a list for pairwise overlap
/// checks.
fn present_regions(frame: super::SectionFrame) -> alloc::vec::Vec<Rect> {
    let mut regions = alloc::vec::Vec::new();
    if let Some(r) = frame.sidebar {
        regions.push(r);
    }
    if !frame.header.is_empty() {
        regions.push(frame.header);
    }
    regions.push(frame.primary);
    if let Some(r) = frame.detail {
        regions.push(r);
    }
    if let Some(r) = frame.impact {
        regions.push(r);
    }
    if let Some(r) = frame.rail {
        regions.push(r);
    }
    if !frame.footer.is_empty() {
        regions.push(frame.footer);
    }
    regions
}

/// No two of `regions` share a pixel.
fn no_overlap(regions: &[Rect]) -> bool {
    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            if !a.intersection(b).is_empty() {
                return false;
            }
        }
    }
    true
}

#[test]
fn every_region_present_when_content_is_generous() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let content = Rect::new(0, 0, 800, 600);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(frame.sidebar.is_some());
    assert!(frame.detail.is_some());
    assert!(frame.impact.is_some());
    assert!(frame.rail.is_some());
    assert_eq!(frame.header.height, anatomy.header_height);
    assert_eq!(frame.footer.height, anatomy.footer_height);
    assert!(!frame.primary.is_empty());
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn impact_sits_between_the_detail_pane_and_the_rail() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let content = Rect::new(0, 0, 800, 600);
    let frame = resolve_section_frame(content, full_anatomy(), scale, &theme);

    let (Some(detail), Some(impact), Some(rail)) = (frame.detail, frame.impact, frame.rail) else {
        panic!("a generous content seats detail, impact and rail");
    };
    assert!(frame.primary.right() <= detail.left());
    assert!(detail.right() <= impact.left());
    assert!(impact.right() <= rail.left());
}

#[test]
fn narrower_content_drops_detail_first() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let gap = theme.metrics().control_gap;
    // Wide enough for sidebar + rail + a sliver of primary, but not detail
    // as well.
    let content = Rect::new(
        0,
        0,
        anatomy.sidebar_width + gap + anatomy.rail_width + gap + 4,
        200,
    );
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(frame.sidebar.is_some(), "sidebar survives this width");
    assert!(frame.detail.is_none(), "detail is the first to drop");
    assert!(frame.impact.is_none(), "impact follows detail out");
    assert!(frame.rail.is_some(), "rail survives once detail is gone");
    assert!(!frame.primary.is_empty());
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn impact_survives_when_only_the_detail_pane_must_drop() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let gap = theme.metrics().control_gap;
    // Wide enough for sidebar + impact + rail + a sliver of primary, but
    // not the detail pane as well.
    let content = Rect::new(
        0,
        0,
        anatomy.sidebar_width + gap + anatomy.impact_width + gap + anatomy.rail_width + gap + 4,
        200,
    );
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(frame.detail.is_none(), "detail is the first to drop");
    assert!(
        frame.impact.is_some(),
        "impact keeps its column while there is room for it"
    );
    assert!(frame.rail.is_some());
    assert!(!frame.primary.is_empty());
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn even_narrower_content_drops_rail_next() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let gap = theme.metrics().control_gap;
    // Wide enough for the sidebar and a sliver of primary, but not detail
    // or rail.
    let content = Rect::new(0, 0, anatomy.sidebar_width + gap + 4, 200);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(frame.sidebar.is_some(), "sidebar survives this width");
    assert!(frame.detail.is_none());
    assert!(frame.impact.is_none());
    assert!(frame.rail.is_none(), "rail is dropped after detail");
    assert!(!frame.primary.is_empty());
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn narrowest_content_drops_sidebar_last() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    // Narrower than the sidebar alone: nothing else can possibly fit.
    let content = Rect::new(0, 0, 4, 200);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(
        frame.sidebar.is_none(),
        "sidebar is dropped last, but it drops"
    );
    assert!(frame.detail.is_none());
    assert!(frame.impact.is_none());
    assert!(frame.rail.is_none());
    assert_eq!(frame.primary.width, content.width);
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn content_narrower_than_sidebar_alone_gives_primary_everything() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = SectionAnatomy {
        sidebar_width: 400,
        ..SectionAnatomy::PRIMARY_ONLY
    };
    let content = Rect::new(0, 0, 40, 200);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(frame.sidebar.is_none());
    assert_eq!(frame.primary.width, content.width);
    assert_eq!(frame.primary.left(), content.left());
}

#[test]
fn primary_never_empty_while_content_has_width() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    for width in [1u32, 2, 4, 8, 16, 32, 64, 128, 256, 1024] {
        let content = Rect::new(0, 0, width, 300);
        let frame = resolve_section_frame(content, anatomy, scale, &theme);
        assert!(
            !frame.primary.is_empty(),
            "primary starved at content width {width}"
        );
    }
}

#[test]
fn zero_width_content_never_panics_and_stays_non_negative() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let content = Rect::new(0, 0, 0, 300);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    // Nothing to seat at all; every region collapses rather than going
    // negative or panicking.
    assert!(frame.sidebar.is_none());
    assert!(frame.detail.is_none());
    assert!(frame.impact.is_none());
    assert!(frame.rail.is_none());
    assert_eq!(frame.primary.width, 0);
}

#[test]
fn zero_height_content_collapses_header_and_footer() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let content = Rect::new(0, 0, 400, 0);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert_eq!(frame.header.height, 0);
    assert_eq!(frame.footer.height, 0);
    assert_eq!(frame.primary.height, 0);
    if let Some(detail) = frame.detail {
        assert_eq!(detail.height, 0);
    }
    if let Some(impact) = frame.impact {
        assert_eq!(impact.height, 0);
    }
    if let Some(rail) = frame.rail {
        assert_eq!(rail.height, 0);
    }
}

#[test]
fn header_and_footer_collapse_to_zero_when_not_requested() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = SectionAnatomy::PRIMARY_ONLY;
    let content = Rect::new(0, 0, 400, 300);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert_eq!(frame.header.height, 0);
    assert_eq!(frame.footer.height, 0);
    assert_eq!(frame.primary.height, content.height);
}

#[test]
fn header_and_footer_clip_rather_than_overflow_a_short_content() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = SectionAnatomy {
        header_height: 40,
        footer_height: 40,
        ..SectionAnatomy::PRIMARY_ONLY
    };
    // Shorter than the header and footer combined.
    let content = Rect::new(0, 0, 400, 30);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert_eq!(
        frame.header.height, 30,
        "header claims all of the height first"
    );
    assert_eq!(frame.footer.height, 0, "nothing is left for the footer");
    assert_eq!(frame.primary.height, 0);
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn header_and_footer_never_overlap_when_both_partly_fit() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = SectionAnatomy {
        header_height: 20,
        footer_height: 20,
        ..SectionAnatomy::PRIMARY_ONLY
    };
    // Room for both header and footer in full, plus a little primary.
    let content = Rect::new(0, 0, 400, 45);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert_eq!(frame.header.height, 20);
    assert_eq!(frame.footer.height, 20);
    assert_eq!(frame.primary.height, 5);
    assert_eq!(frame.header.bottom(), frame.primary.top());
    assert_eq!(frame.primary.bottom(), frame.footer.top());
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn regions_never_overlap_across_a_spread_of_widths() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    for width in [4u32, 20, 60, 120, 200, 400, 900] {
        let content = Rect::new(0, 0, width, 300);
        let frame = resolve_section_frame(content, anatomy, scale, &theme);
        assert!(
            no_overlap(&present_regions(frame)),
            "regions overlapped at content width {width}"
        );
    }
}

#[test]
fn minimum_width_is_enough_to_seat_every_region() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = full_anatomy();
    let width = anatomy.minimum_width(scale, &theme);
    let content = Rect::new(0, 0, width, 300);
    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    assert!(frame.sidebar.is_some());
    assert!(frame.detail.is_some());
    assert!(frame.impact.is_some());
    assert!(frame.rail.is_some());
    assert!(!frame.primary.is_empty());
}

#[test]
fn minimum_width_of_primary_only_anatomy_is_one() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    assert_eq!(SectionAnatomy::PRIMARY_ONLY.minimum_width(scale, &theme), 1);
}

#[test]
fn minimum_height_of_primary_only_anatomy_is_one() {
    let scale = Scale::ONE;
    assert_eq!(SectionAnatomy::PRIMARY_ONLY.minimum_height(scale), 1);
}

/// [`full_anatomy`] with a four-command row strip in its primary column — the
/// shape a flattened group list with inline commands per header row declares.
fn commanded_anatomy() -> SectionAnatomy {
    SectionAnatomy {
        primary_row_commands: 4,
        ..full_anatomy()
    }
}

#[test]
fn primary_is_never_narrower_than_its_declared_floor() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = commanded_anatomy();
    let floor = anatomy.primary_floor(scale, &theme);
    // Every width from "cannot even seat the floor" upwards: while the content
    // is wide enough for the floor at all, primary has it.
    for width in [floor, floor + 1, floor + 40, floor + 200, floor + 900] {
        let content = Rect::new(0, 0, width, 300);
        let frame = resolve_section_frame(content, anatomy, scale, &theme);
        assert!(
            frame.primary.width >= floor,
            "primary fell to {} below its floor {floor} at content width {width}",
            frame.primary.width
        );
        assert!(no_overlap(&present_regions(frame)));
    }
}

#[test]
fn a_declared_floor_sheds_the_optional_columns_in_order() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = commanded_anatomy();
    let gap = theme.metrics().control_gap;
    let floor = anatomy.primary_floor(scale, &theme);
    let optional = anatomy.sidebar_width
        + gap
        + anatomy.detail_width
        + gap
        + anatomy.impact_width
        + gap
        + anatomy.rail_width
        + gap;

    // Enough for the floor and every optional column: nothing is shed.
    let frame = resolve_section_frame(
        Rect::new(0, 0, floor + optional, 300),
        anatomy,
        scale,
        &theme,
    );
    assert!(frame.sidebar.is_some());
    assert!(
        frame.detail.is_some(),
        "nothing sheds while the floor is met"
    );
    assert!(frame.impact.is_some());
    assert!(frame.rail.is_some());

    // One pixel short of that: the detail pane goes first, and its width comes
    // back to primary rather than being taken off the floor.
    let frame = resolve_section_frame(
        Rect::new(0, 0, floor + optional - 1, 300),
        anatomy,
        scale,
        &theme,
    );
    assert!(frame.detail.is_none(), "detail sheds first for the floor");
    assert!(
        frame.impact.is_some(),
        "impact keeps its column while it can"
    );
    assert!(frame.rail.is_some());
    assert!(frame.primary.width >= floor);

    // Short of the floor plus sidebar, impact and rail: impact follows.
    let width =
        floor + anatomy.sidebar_width + gap + anatomy.impact_width + gap + anatomy.rail_width + gap
            - 1;
    let frame = resolve_section_frame(Rect::new(0, 0, width, 300), anatomy, scale, &theme);
    assert!(frame.detail.is_none());
    assert!(frame.impact.is_none(), "impact sheds after detail");
    assert!(frame.rail.is_some(), "the rail outlasts both of them");
    assert!(frame.primary.width >= floor);

    // Short of the floor plus sidebar and rail: the rail goes, sidebar last.
    let width = floor + anatomy.sidebar_width + gap + anatomy.rail_width + gap - 1;
    let frame = resolve_section_frame(Rect::new(0, 0, width, 300), anatomy, scale, &theme);
    assert!(frame.rail.is_none(), "the rail sheds after impact");
    assert!(frame.sidebar.is_some(), "the sidebar is shed last of all");
    assert!(frame.primary.width >= floor);

    // Short of the floor plus the sidebar: the sidebar goes too.
    let width = floor + anatomy.sidebar_width + gap - 1;
    let frame = resolve_section_frame(Rect::new(0, 0, width, 300), anatomy, scale, &theme);
    assert!(frame.sidebar.is_none());
    assert_eq!(frame.primary.width, width);
    assert!(frame.primary.width >= floor);
}

#[test]
fn a_floor_larger_than_the_whole_content_still_leaves_a_usable_primary() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = commanded_anatomy();
    let floor = anatomy.primary_floor(scale, &theme);
    let content = Rect::new(0, 0, floor / 2, 300);

    let frame = resolve_section_frame(content, anatomy, scale, &theme);

    // Nothing left to shed, so primary keeps the whole width rather than
    // holding out for a floor the content cannot give it.
    assert!(frame.sidebar.is_none());
    assert!(frame.detail.is_none());
    assert!(frame.impact.is_none());
    assert!(frame.rail.is_none());
    assert_eq!(frame.primary.width, content.width);
    assert_eq!(frame.primary.left(), content.left());
    assert!(!frame.primary.is_empty());
    assert!(no_overlap(&present_regions(frame)));
}

#[test]
fn the_floor_is_the_command_strip_the_row_splitter_lays_out() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let m = theme.metrics();

    // A row's strip is its commands, the gap that keeps them off the row's
    // text, and the row's trailing inset — no more, so no column is shed for
    // width nothing draws in.
    for commands in [1u32, 2, 4, 8] {
        let anatomy = SectionAnatomy {
            primary_row_commands: commands,
            ..SectionAnatomy::PRIMARY_ONLY
        };
        assert_eq!(
            anatomy.primary_floor(scale, &theme),
            super::row_commands_width(commands, scale, &theme)
                + scale.scale_length(m.control_gap)
                + scale.scale_length(m.control_inset)
        );
    }

    // A section whose rows carry no commands floors at the one pixel primary
    // needs to exist at all, so it sheds nothing on its account.
    assert_eq!(SectionAnatomy::PRIMARY_ONLY.primary_floor(scale, &theme), 1);
}

#[test]
fn minimum_width_counts_the_declared_floor_not_a_bare_pixel() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let anatomy = commanded_anatomy();

    // Seating everything means seating the floor as well, so the minimum grows
    // with the strip rather than assuming a one-pixel primary will do.
    let width = anatomy.minimum_width(scale, &theme);
    assert_eq!(
        width,
        full_anatomy().minimum_width(scale, &theme) - 1 + anatomy.primary_floor(scale, &theme)
    );

    let frame = resolve_section_frame(Rect::new(0, 0, width, 300), anatomy, scale, &theme);
    assert!(frame.sidebar.is_some());
    assert!(frame.detail.is_some());
    assert!(frame.impact.is_some());
    assert!(frame.rail.is_some());
    assert!(frame.primary.width >= anatomy.primary_floor(scale, &theme));
}

// --- The location band ---------------------------------------------------

/// A band the width of a comfortable window, one control high.
fn band(width: u32, theme: &Theme) -> Rect {
    Rect::new(
        0,
        0,
        width,
        Scale::ONE.scale_length(theme.metrics().control_height),
    )
}

#[test]
fn a_band_with_no_summary_gives_the_trail_everything_but_the_command() {
    let theme = Theme::dark();
    let rect = band(600, &theme);
    let layout = resolve_band(rect, None, Scale::ONE, &theme);

    assert_eq!(layout.summary, None, "no summary was asked for");
    assert_eq!(
        layout.command,
        Rect::new(
            rect.right() - i32::try_from(rect.height).unwrap_or(0),
            rect.top(),
            rect.height,
            rect.height
        ),
        "the command is a square at the trailing edge"
    );
    assert_eq!(layout.trail.left(), rect.left());
    assert!(layout.trail.right() <= layout.command.left());
}

#[test]
fn a_summary_is_seated_between_the_trail_and_the_command() {
    let theme = Theme::dark();
    let rect = band(600, &theme);
    let layout = resolve_band(
        rect,
        Some(BandSummary {
            width: 240,
            height: 52,
        }),
        Scale::ONE,
        &theme,
    );

    let summary = layout.summary.expect("a summary this wide band can seat");
    assert_eq!(summary.width, 240);
    assert!(
        layout.trail.right() <= summary.left(),
        "the trail comes first and does not overlap the summary"
    );
    assert!(
        summary.right() <= layout.command.left(),
        "and the summary stops short of the command"
    );
    assert_eq!(summary.height, rect.height, "it fills the band's height");
}

#[test]
fn a_band_too_narrow_for_the_summary_keeps_the_trail_whole() {
    let theme = Theme::dark();
    let rect = band(220, &theme);
    let layout = resolve_band(
        rect,
        Some(BandSummary {
            width: 240,
            height: 52,
        }),
        Scale::ONE,
        &theme,
    );

    assert_eq!(
        layout.summary, None,
        "the reader's own location outranks a census they can read elsewhere"
    );
    assert_eq!(layout.trail.left(), rect.left());
    assert!(layout.trail.right() <= layout.command.left());
}

#[test]
fn the_band_grows_only_for_a_section_that_asks_it_to() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let resting = scale.scale_length(theme.metrics().control_height);

    assert_eq!(
        SectionAnatomy::PRIMARY_ONLY.band_height(scale, &theme),
        resting,
        "a section with no census pays for none"
    );

    let anatomy = SectionAnatomy {
        band_summary: Some(BandSummary {
            width: 240,
            height: 52,
        }),
        ..SectionAnatomy::PRIMARY_ONLY
    };
    assert_eq!(anatomy.band_height(scale, &theme), scale.scale_length(52));
}
