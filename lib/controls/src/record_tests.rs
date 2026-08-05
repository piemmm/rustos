//! Unit tests for the record-list family.
//!
//! These cover construction, `row_height`/`measured_height` agreeing with
//! what `render` actually lays out, right-aligned toned values and every
//! [`SignalRole`] tone for [`FactList`], separators drawn between but never
//! after rows, the label truncating before the value under a narrow width,
//! the [`Timeline`] spine spanning only the first-to-last mark (and absent
//! for a single event), [`EventMark::Notable`] versus [`EventMark::Routine`]
//! differing in the rendered pixels, the stamp column aligning on the widest
//! stamp, event-text truncation, rows omitted (never clipped) when the
//! height runs out, an empty collection painting nothing for either control,
//! degenerate bounds, both built-in themes, and `crate::testkit::high_contrast()`.

use alloc::vec;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, SignalRole, Theme};

use crate::record::{EventMark, Fact, FactList, Timeline, TimelineEvent};
use crate::testkit::high_contrast;

const W: u32 = 320;

fn font() -> BitmapFont {
    BitmapFont::console()
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// The bounding box `(min_x, min_y, max_x, max_y)` of `want` in `surface`.
fn bbox(surface: &Surface, want: Pixel) -> Option<(u32, u32, u32, u32)> {
    let mut found: Option<(u32, u32, u32, u32)> = None;
    for y in 0..surface.height() {
        for x in 0..surface.width() {
            if surface.get(x, y) != Some(want) {
                continue;
            }
            found = Some(match found {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
    }
    found
}

/// Whether any pixel of row `y` equals `want`.
fn row_has(surface: &Surface, y: u32, want: Pixel) -> bool {
    (0..surface.width()).any(|x| surface.get(x, y) == Some(want))
}

/// The ten [`SignalRole`] tones a toned value or mark may take.
fn signal_roles() -> [SignalRole; 10] {
    [
        SignalRole::Cpu,
        SignalRole::Memory,
        SignalRole::Disk,
        SignalRole::Network,
        SignalRole::Power,
        SignalRole::Thermal,
        SignalRole::Recovery,
        SignalRole::Success,
        SignalRole::Warning,
        SignalRole::Denied,
    ]
}

// --- FactList -------------------------------------------------------------

fn fact_surface(list: &FactList, theme: &Theme, scale: Scale, font: BitmapFont) -> Surface {
    let h = list.measured_height(scale, theme, font);
    let mut surface = Surface::new(W, h.max(1)).expect("surface");
    list.render(&mut surface, Rect::new(0, 0, W, h), scale, theme, font);
    surface
}

#[test]
fn fact_builders_set_expected_fields() {
    let fact = Fact::new("Label", "Value").with_tone(SignalRole::Warning);
    assert_eq!(fact.label(), "Label");
    assert_eq!(fact.value(), "Value");
    assert_eq!(fact.tone(), Some(SignalRole::Warning));

    let untoned = Fact::new("A", "B");
    assert_eq!(untoned.tone(), None);
}

#[test]
fn fact_list_construction_reports_its_facts() {
    let list = FactList::new(vec![Fact::new("A", "1"), Fact::new("B", "2")]);
    assert_eq!(list.len(), 2);
    assert!(!list.is_empty());
    assert_eq!(list.facts()[0].label(), "A");
    assert_eq!(list.facts()[1].value(), "2");

    let empty = FactList::new(Vec::new());
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
}

#[test]
fn fact_list_measured_height_matches_the_row_count_render_lays_out() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let row = FactList::row_height(scale, &theme, font);
    let list = FactList::new(vec![
        Fact::new("A", "1"),
        Fact::new("B", "2"),
        Fact::new("C", "3"),
    ]);
    assert_eq!(list.measured_height(scale, &theme, font), row * 3);

    // Every row fits within exactly that height: both the muted label
    // foreground and the emphasised value foreground appear.
    let surface = fact_surface(&list, &theme, scale, font);
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn fact_list_measured_height_grows_with_scale() {
    let theme = Theme::dark();
    let font = font();
    let list = FactList::new(vec![Fact::new("A", "1")]);
    let unit = list.measured_height(Scale::ONE, &theme, font);
    let doubled =
        list.measured_height(Scale::from_percent(200).expect("valid scale"), &theme, font);
    assert!(doubled > unit, "a larger scale must need more height");
}

#[test]
fn fact_values_are_right_aligned() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![Fact::new("CPU Usage", "42").with_tone(SignalRole::Cpu)]);
    let surface = fact_surface(&list, &theme, scale, font);
    let tone = premul(theme.palette().signal(SignalRole::Cpu));
    let (_, _, max_x, _) = bbox(&surface, tone).expect("value drawn");
    assert_eq!(max_x, W - 1, "the value must sit flush with the right edge");
}

#[test]
fn every_tone_resolves_its_own_value_colour() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    for role in signal_roles() {
        let list = FactList::new(vec![Fact::new("L", "V").with_tone(role)]);
        let surface = fact_surface(&list, &theme, scale, font);
        let expected = premul(theme.palette().signal(role));
        assert!(
            has_pixel(&surface, expected),
            "{role:?} must tint the value"
        );
    }
}

#[test]
fn an_untoned_value_takes_the_plain_foreground() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![Fact::new("L", "V")]);
    let surface = fact_surface(&list, &theme, scale, font);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn the_label_truncates_before_the_value_under_a_narrow_width() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let cell = font.cell_width();
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let label = "LABELLABEL";
    let value = "OK";
    let list = FactList::new(vec![Fact::new(label, value)]);
    let value_w = font.text_width(value);
    let row_h = FactList::row_height(scale, &theme, font);
    let label_color = premul(theme.palette().on_surface_muted);
    let value_color = premul(theme.palette().on_surface);

    // Exactly three label characters' worth of room after the value and gap.
    let narrow_w = value_w + gap + cell * 3;
    let mut narrow = Surface::new(narrow_w, row_h).expect("surface");
    list.render(
        &mut narrow,
        Rect::new(0, 0, narrow_w, row_h),
        scale,
        &theme,
        font,
    );
    let narrow_label_w = bbox(&narrow, label_color).map_or(0, |(x0, _, x1, _)| x1 - x0 + 1);
    assert_eq!(
        narrow_label_w,
        cell * 3,
        "the label must truncate to exactly the width it is given"
    );
    let (_, _, narrow_value_max_x, _) = bbox(&narrow, value_color).expect("value drawn");
    assert_eq!(
        narrow_value_max_x,
        narrow_w - 1,
        "the value keeps its full width and stays right-aligned"
    );

    // Ample room: the whole label draws.
    let full_w = value_w + gap + font.text_width(label) + gap.saturating_mul(4);
    let mut full = Surface::new(full_w, row_h).expect("surface");
    list.render(
        &mut full,
        Rect::new(0, 0, full_w, row_h),
        scale,
        &theme,
        font,
    );
    let full_label_w = bbox(&full, label_color).map_or(0, |(x0, _, x1, _)| x1 - x0 + 1);
    assert_eq!(
        full_label_w,
        font.text_width(label),
        "the full label draws when there is room for it"
    );
}

#[test]
fn separators_draw_between_rows_but_never_after_the_last() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![
        Fact::new("A", "1"),
        Fact::new("B", "2"),
        Fact::new("C", "3"),
    ])
    .with_separators(true);
    let row_h = FactList::row_height(scale, &theme, font);
    let line_h = font.line_height();
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let sep_thickness = crate::paint::plate_border(&theme, scale).min(gap);
    let offset = gap.saturating_sub(sep_thickness) / 2;
    let rim = premul(theme.palette().rim);

    let h = row_h * 3;
    let mut surface = Surface::new(W, h).expect("surface");
    list.render(&mut surface, Rect::new(0, 0, W, h), scale, &theme, font);

    let sep_y = |row: u32| row * row_h + line_h + offset;
    assert!(
        row_has(&surface, sep_y(0), rim),
        "a rule must sit after row 0"
    );
    assert!(
        row_has(&surface, sep_y(1), rim),
        "a rule must sit after row 1"
    );
    assert!(
        !row_has(&surface, sep_y(2), rim),
        "no rule may sit after the last row"
    );
}

#[test]
fn no_separator_draws_when_disabled() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![Fact::new("A", "1"), Fact::new("B", "2")]);
    let surface = fact_surface(&list, &theme, scale, font);
    assert!(!has_pixel(&surface, premul(theme.palette().rim)));
}

#[test]
fn fact_rows_are_omitted_rather_than_clipped_when_the_height_runs_out() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![
        Fact::new("A", "1").with_tone(SignalRole::Cpu),
        Fact::new("B", "2").with_tone(SignalRole::Memory),
        Fact::new("C", "3").with_tone(SignalRole::Disk),
    ]);
    let row_h = FactList::row_height(scale, &theme, font);
    let line_h = font.line_height();
    // Room for two whole rows, but short of a third's text line.
    let h = row_h * 2 + line_h.saturating_sub(1);
    let mut surface = Surface::new(W, h).expect("surface");
    list.render(&mut surface, Rect::new(0, 0, W, h), scale, &theme, font);

    assert!(has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Cpu))
    ));
    assert!(has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Memory))
    ));
    assert!(!has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Disk))
    ));
}

#[test]
fn an_empty_fact_list_paints_nothing() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(Vec::new());
    assert_eq!(list.measured_height(scale, &theme, font), 0);

    let fill = Color::rgb(12, 34, 56).premultiply();
    let mut surface = Surface::filled(W, 40, fill).expect("surface");
    let before = surface.clone();
    list.render(&mut surface, Rect::new(0, 0, W, 40), scale, &theme, font);
    assert_eq!(surface, before);
}

#[test]
fn fact_list_degenerate_bounds_do_not_panic() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![Fact::new("A", "1")]);
    let mut surface = Surface::new(8, 8).expect("surface");
    list.render(&mut surface, Rect::new(0, 0, 0, 8), scale, &theme, font);
    list.render(&mut surface, Rect::new(0, 0, 8, 0), scale, &theme, font);
    list.render(&mut surface, Rect::new(0, 0, 1, 8), scale, &theme, font);
}

#[test]
fn fact_list_renders_in_both_themes() {
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![
        Fact::new("Memory", "8.6 GB").with_tone(SignalRole::Memory)
    ]);
    let dark = fact_surface(&list, &Theme::dark(), scale, font);
    let light = fact_surface(&list, &Theme::light(), scale, font);
    assert_ne!(dark.pixels(), light.pixels());
}

#[test]
fn fact_list_high_contrast_changes_the_separator_rendering() {
    let font = font();
    let scale = Scale::ONE;
    let list = FactList::new(vec![Fact::new("A", "1"), Fact::new("B", "2")]).with_separators(true);
    let normal = fact_surface(&list, &Theme::dark(), scale, font);
    let heavy = fact_surface(&list, &high_contrast(), scale, font);
    assert_ne!(normal.pixels(), heavy.pixels());
}

// --- Timeline ---------------------------------------------------------

fn timeline_surface(timeline: &Timeline, theme: &Theme, scale: Scale, font: BitmapFont) -> Surface {
    let h = timeline.measured_height(scale, theme, font);
    let mut surface = Surface::new(W, h.max(1)).expect("surface");
    timeline.render(&mut surface, Rect::new(0, 0, W, h), scale, theme, font);
    surface
}

/// The y coordinate of row `row`'s mark centre, following exactly the same
/// arithmetic [`Timeline::render`] uses.
fn row_center(row: u32, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
    let row_h = Timeline::row_height(scale, theme, font);
    let line_h = font.line_height();
    row * row_h + line_h / 2
}

fn mark_radius(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().bead_size).max(1) / 2
}

#[test]
fn timeline_event_builders_set_expected_fields() {
    let event = TimelineEvent::new("09:00", "Started")
        .with_mark(EventMark::Notable)
        .with_tone(SignalRole::Success);
    assert_eq!(event.stamp(), "09:00");
    assert_eq!(event.text(), "Started");
    assert_eq!(event.mark(), EventMark::Notable);

    let default_mark = TimelineEvent::new("a", "b");
    assert_eq!(default_mark.mark(), EventMark::Routine);
}

#[test]
fn timeline_construction_reports_its_events() {
    let timeline = Timeline::new(vec![
        TimelineEvent::new("09:00", "Started"),
        TimelineEvent::new("10:00", "Finished"),
    ]);
    assert_eq!(timeline.len(), 2);
    assert!(!timeline.is_empty());
    assert_eq!(timeline.events()[0].stamp(), "09:00");

    let empty = Timeline::new(Vec::new());
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
}

#[test]
fn timeline_measured_height_matches_the_row_count_render_lays_out() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let row = Timeline::row_height(scale, &theme, font);
    let timeline = Timeline::new(vec![
        TimelineEvent::new("09:00", "Started"),
        TimelineEvent::new("10:00", "Finished"),
    ]);
    assert_eq!(timeline.measured_height(scale, &theme, font), row * 2);
    assert!(Timeline::gutter_width(scale, &theme) > 0);
}

#[test]
fn timeline_measured_height_grows_with_scale() {
    let theme = Theme::dark();
    let font = font();
    let timeline = Timeline::new(vec![TimelineEvent::new("09:00", "Started")]);
    let unit = timeline.measured_height(Scale::ONE, &theme, font);
    let doubled =
        timeline.measured_height(Scale::from_percent(200).expect("valid scale"), &theme, font);
    assert!(doubled > unit, "a larger scale must need more height");
}

#[test]
fn the_spine_is_absent_for_a_single_event() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![TimelineEvent::new("09:00", "Started")
        .with_mark(EventMark::Notable)
        .with_tone(SignalRole::Cpu)]);
    let surface = timeline_surface(&timeline, &theme, scale, font);
    assert!(
        !has_pixel(&surface, premul(theme.palette().rim)),
        "a single event has nothing to connect, so no spine may draw"
    );
    assert!(has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Cpu))
    ));
}

#[test]
fn the_spine_spans_only_from_the_first_mark_to_the_last() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    // Notable, toned marks never paint the rim colour themselves, so every
    // rim pixel found below can only be the spine.
    let timeline = Timeline::new(vec![
        TimelineEvent::new("09:00", "A")
            .with_mark(EventMark::Notable)
            .with_tone(SignalRole::Cpu),
        TimelineEvent::new("10:00", "B")
            .with_mark(EventMark::Notable)
            .with_tone(SignalRole::Cpu),
        TimelineEvent::new("11:00", "C")
            .with_mark(EventMark::Notable)
            .with_tone(SignalRole::Cpu),
    ]);
    let surface = timeline_surface(&timeline, &theme, scale, font);
    let rim = premul(theme.palette().rim);
    let radius = mark_radius(scale, &theme);
    let spine_x = radius;

    let first_center = row_center(0, scale, &theme, font);
    let second_center = row_center(1, scale, &theme, font);
    let last_center = row_center(2, scale, &theme, font);
    // Strictly outside every mark's own circle, so a pixel found here can
    // only ever be the spine (or nothing).
    let above_first_mark = first_center.saturating_sub(radius).saturating_sub(1);
    let below_last_mark = last_center.saturating_add(radius).saturating_add(1);
    let between_first_and_second = first_center / 2 + second_center / 2;

    assert_eq!(
        surface.get(spine_x, above_first_mark),
        Some(Pixel::TRANSPARENT),
        "the spine must not reach above the first mark"
    );
    assert_eq!(
        surface.get(spine_x, below_last_mark),
        Some(Pixel::TRANSPARENT),
        "the spine must not reach below the last mark"
    );
    assert_eq!(
        surface.get(spine_x, between_first_and_second),
        Some(rim),
        "the spine must connect the marks in between"
    );
}

#[test]
fn notable_and_routine_marks_differ_in_pixels() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let notable = Timeline::new(vec![
        TimelineEvent::new("09:00", "A").with_mark(EventMark::Notable)
    ]);
    let routine = Timeline::new(vec![
        TimelineEvent::new("09:00", "A").with_mark(EventMark::Routine)
    ]);
    let notable_surface = timeline_surface(&notable, &theme, scale, font);
    let routine_surface = timeline_surface(&routine, &theme, scale, font);
    assert_ne!(
        notable_surface.pixels(),
        routine_surface.pixels(),
        "the two marks must differ in shape, not merely in colour"
    );
}

#[test]
fn every_tone_resolves_its_own_notable_mark_colour() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    for role in signal_roles() {
        let timeline = Timeline::new(vec![TimelineEvent::new("09:00", "A")
            .with_mark(EventMark::Notable)
            .with_tone(role)]);
        let surface = timeline_surface(&timeline, &theme, scale, font);
        let expected = premul(theme.palette().signal(role));
        assert!(
            has_pixel(&surface, expected),
            "{role:?} must tint the notable mark"
        );
    }
}

#[test]
fn an_untoned_notable_mark_takes_the_accent() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![
        TimelineEvent::new("09:00", "A").with_mark(EventMark::Notable)
    ]);
    let surface = timeline_surface(&timeline, &theme, scale, font);
    assert!(has_pixel(&surface, premul(theme.palette().accent)));
}

#[test]
fn the_stamp_column_aligns_on_the_widest_stamp() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![
        TimelineEvent::new("1", "X"),
        TimelineEvent::new("1234567890", "X"),
    ]);
    let surface = timeline_surface(&timeline, &theme, scale, font);
    let text_color = premul(theme.palette().on_surface);
    let row_h = Timeline::row_height(scale, &theme, font);

    let leftmost = |y: u32| (0..surface.width()).find(|&x| surface.get(x, y) == Some(text_color));
    let row0 = leftmost(0);
    let row1 = leftmost(row_h);
    assert!(row0.is_some(), "the first row's text must draw");
    assert_eq!(
        row0, row1,
        "every row's text must start at the same column, whatever its own stamp's length"
    );
}

#[test]
fn event_text_truncates_when_the_width_runs_out() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let long_text = "AAAAAAAAAAAAAAAAAAAA";
    let timeline = Timeline::new(vec![TimelineEvent::new("1", long_text)]);
    let row_h = Timeline::row_height(scale, &theme, font);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let gutter_w = Timeline::gutter_width(scale, &theme);
    let stamp_w = font.text_width("1");
    let text_x = gutter_w + gap + stamp_w + gap;
    let cell = font.cell_width();
    let narrow_w = text_x + cell * 3;

    let mut surface = Surface::new(narrow_w, row_h).expect("surface");
    timeline.render(
        &mut surface,
        Rect::new(0, 0, narrow_w, row_h),
        scale,
        &theme,
        font,
    );
    let text_color = premul(theme.palette().on_surface);
    let (_, _, max_x, _) = bbox(&surface, text_color).expect("text drawn");
    assert_eq!(
        max_x,
        narrow_w - 1,
        "the text must truncate to exactly the width it is given"
    );
}

#[test]
fn timeline_rows_are_omitted_rather_than_clipped_when_the_height_runs_out() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![
        TimelineEvent::new("1", "A")
            .with_mark(EventMark::Notable)
            .with_tone(SignalRole::Cpu),
        TimelineEvent::new("2", "B")
            .with_mark(EventMark::Notable)
            .with_tone(SignalRole::Memory),
        TimelineEvent::new("3", "C")
            .with_mark(EventMark::Notable)
            .with_tone(SignalRole::Disk),
    ]);
    let row_h = Timeline::row_height(scale, &theme, font);
    let line_h = font.line_height();
    let h = row_h * 2 + line_h.saturating_sub(1);
    let mut surface = Surface::new(W, h).expect("surface");
    timeline.render(&mut surface, Rect::new(0, 0, W, h), scale, &theme, font);

    assert!(has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Cpu))
    ));
    assert!(has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Memory))
    ));
    assert!(!has_pixel(
        &surface,
        premul(theme.palette().signal(SignalRole::Disk))
    ));
}

#[test]
fn an_empty_timeline_paints_nothing() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(Vec::new());
    assert_eq!(timeline.measured_height(scale, &theme, font), 0);

    let fill = Color::rgb(65, 43, 21).premultiply();
    let mut surface = Surface::filled(W, 40, fill).expect("surface");
    let before = surface.clone();
    timeline.render(&mut surface, Rect::new(0, 0, W, 40), scale, &theme, font);
    assert_eq!(surface, before);
}

#[test]
fn timeline_degenerate_bounds_do_not_panic() {
    let theme = Theme::dark();
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![
        TimelineEvent::new("1", "A"),
        TimelineEvent::new("2", "B"),
    ]);
    let mut surface = Surface::new(8, 40).expect("surface");
    timeline.render(&mut surface, Rect::new(0, 0, 0, 40), scale, &theme, font);
    timeline.render(&mut surface, Rect::new(0, 0, 8, 0), scale, &theme, font);
    let gutter_w = Timeline::gutter_width(scale, &theme);
    let narrower_than_the_gutter = gutter_w.saturating_sub(1).max(1);
    timeline.render(
        &mut surface,
        Rect::new(0, 0, narrower_than_the_gutter, 40),
        scale,
        &theme,
        font,
    );
}

#[test]
fn timeline_renders_in_both_themes() {
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![
        TimelineEvent::new("09:00", "Started").with_tone(SignalRole::Cpu)
    ]);
    let dark = timeline_surface(&timeline, &Theme::dark(), scale, font);
    let light = timeline_surface(&timeline, &Theme::light(), scale, font);
    assert_ne!(dark.pixels(), light.pixels());
}

#[test]
fn timeline_high_contrast_changes_the_routine_ring_rendering() {
    let font = font();
    let scale = Scale::ONE;
    let timeline = Timeline::new(vec![
        TimelineEvent::new("09:00", "A"),
        TimelineEvent::new("10:00", "B"),
    ]);
    let normal = timeline_surface(&timeline, &Theme::dark(), scale, font);
    let heavy = timeline_surface(&timeline, &high_contrast(), scale, font);
    assert_ne!(normal.pixels(), heavy.pixels());
}
