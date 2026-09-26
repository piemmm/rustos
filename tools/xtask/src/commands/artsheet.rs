//! `cargo xtask artsheet` implementation.
//!
//! The figure engine's art gate. It walks the shared reference grid
//! (`tairix_wintersun_figure::reference`) — each species' reference figure
//! in every shipped motion, and each species' least, most and two plausible
//! figures walking, at eight phases, facing four ways — renders each cell,
//! measures it, and holds every number against a bound. A reference figure
//! is measured at the three pixel sides the desktop draws a figure at; a
//! walking one at the smallest, which is the readability floor, since what a
//! figure costs is counted in outline points and fill area and neither
//! depends on the side.
//!
//! # Why the golden is a ledger and not a picture
//!
//! A committed PNG reproduces the objection the whole plan is written
//! against: a reviewer cannot read a binary diff, so a regression is
//! invisible until somebody opens the file, and git carries the churn on
//! every rig, clip, shape or palette change. The committed golden is
//! therefore a **text ledger** — identity, a pixel digest, and every
//! measured number per cell — where a change reads as
//! `skate 0.002718 -> 0.014803` in the diff. `--sheets` renders the
//! pictures on demand into the gitignored `images/`, so what a human looks
//! at is always current rather than as-of-last-regeneration.
//!
//! The bare form does both halves: it regenerates the ledger and compares
//! it byte for byte (drift), and it checks the freshly measured numbers
//! against their bounds (correctness). Drift without bounds would admit a
//! regression somebody had regenerated; bounds without drift would admit a
//! change nobody noticed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tairix_hash::FastHash;
use tairix_raster::surface::Surface;
use tairix_raster::Color;
use tairix_theme::Theme;
use tairix_wintersun_figure::digest as figure_digest;
use tairix_wintersun_figure::humanoid;
use tairix_wintersun_figure::identity::{Identity, RECORD_EXTENSION, RECORD_LEN};
use tairix_wintersun_figure::mesh::{self, LEVELS};
use tairix_wintersun_figure::motion::Kind;
use tairix_wintersun_figure::paint::{self, Brush, Veil, MAX_FIGURE_POINTS};
use tairix_wintersun_figure::quality::Measured;
use tairix_wintersun_figure::reference::{
    self, Cell, Figure, Reference, Sampling, FACINGS, PHASES, SIDES,
};
use tairix_wintersun_figure::rig::{Placement, Rig};
use tairix_wintersun_figure::rigging::Rigging;
use tairix_wintersun_figure::species::{DYES, TROUSERS};
use tairix_wintersun_figure::tint::Tint;

mod png;

/// The committed ledger, workspace-relative.
pub const LEDGER_PATH: &str = "userland/games/wintersun/figure/artsheet.ledger";

/// Where the game ships its figure presets, workspace-relative: every file
/// there carrying the record extension is one record, and every one is
/// measured.
const PRESETS_DIR: &str = "userland/games/wintersun/app/Resources";

/// Where `--sheets` writes the contact sheets, workspace-relative.
///
/// Gitignored output, like every other image this build produces.
const SHEETS_DIR: &str = "images/artsheet";

/// The band the alpha-weighted coverage ratio must fall inside.
///
/// A figure that fills its box reads as a blob and one that barely marks it
/// reads as nothing. The grid runs from about a sixteenth to about a sixth,
/// so the band is wide either way rather than fitted to it.
///
/// Taken at the scale a standing figure is drawn in the cell: a motion whose
/// raised arms need more room is framed smaller, and its figure is restated
/// at the standing scale rather than read as marking less of its box.
const COVERAGE: (f64, f64) = (0.05, 0.30);

/// The fewest distinct tonal regions a cell must resolve into.
///
/// The concrete form of "the head and limbs stay separable rather than
/// merging into the trunk": a region is a connected run of pixels the
/// painter left at exactly one of the rig's declared tones, so two parts in
/// one tone that touch count once — which is right, because the eye cannot
/// tell them apart either. A figure that has become a blob resolves into
/// one.
///
/// Three is the requirement rather than the measurement: at the smallest
/// size a figure is drawn, the head, the trunk and the legs must each still
/// be a mass of their own. The grid's worst cell — its least beastkin, pale
/// cloth on pale fur, seen from behind at the floor — meets it exactly, in
/// the one dye of the sixteen that does. Every other dye on every species'
/// palest and darkest build resolves into more, and
/// [`every_dye_stays_readable`] holds each of them to the bound.
const MIN_REGIONS: u32 = 3;

/// The fewest pixels a run must hold to count as a region.
///
/// A single stray pixel between two antialiased edges is not something a
/// player can read.
const MIN_REGION_PIXELS: u32 = 2;

/// How covered a pixel must be to be counted as one of the figure's tones.
///
/// Half. Below that a pixel is mostly the ground behind the figure, and
/// which tone it leans toward says nothing about what a player can see.
const MIN_TONE_ALPHA: u8 = 128;

/// How far a pixel may sit from a declared shade and still be read as it.
///
/// Stated as a squared distance over the three channels. A shaded figure is
/// drawn in every tone of a ladder, and neighbouring strips antialias into
/// one another, so demanding an exact match would classify almost nothing
/// at icon size and measure the antialiasing rather than the figure. A
/// pixel that is nothing like any of the tones — a blend across two
/// different parts and the ground — is left unclassified, which is what
/// keeps this a measurement of separable masses rather than a colour
/// search.
const TONE_SLACK: u32 = 48 * 48 * 3;

/// The least contrast the figure must reach against *either* theme's
/// desktop, as a WCAG ratio.
///
/// Measured as the best any substantial tone of the figure reaches rather
/// than what its mean does, because the mean is the wrong statistic: a
/// figure drawn in pale skin and dark cloth has a mid-grey mean and stands
/// out against both backgrounds, while a flat mid-grey figure has the same
/// mean and is invisible against neither-quite. What makes a silhouette
/// readable is that *something* substantial in it separates from the ground.
/// Every figure clears it on its trousers alone, whatever its palette — at
/// a little over two and a fifth against either desktop — which the harness
/// checks of that tone before any cell.
const MIN_CONTRAST: f64 = 2.0;

/// How much of the figure a tone must cover to count toward its contrast.
///
/// A two-pixel highlight is not what a player sees the figure by.
const MIN_TONE_SHARE: f64 = 0.10;

/// The most of a cell the figure may fill, summed over every shape.
///
/// The overdraw the scan converter actually pays: a figure whose parts
/// overlapped many times over would be the frame's cost centre at the size
/// it is largest. The grid's heaviest figure fills about a fifth of its
/// cell.
const MAX_OVERDRAW: f64 = 0.5;

/// The furthest a foot may end up from the ground it was asked for.
///
/// The grid stands on level ground, so any miss at all is the planting solve
/// failing to reproduce the clip.
const MAX_MISS: f64 = 1e-6;

/// Regenerate the committed ledger.
///
/// # Errors
///
/// A measurement that breaches its bound, or a ledger that cannot be
/// written.
pub fn write(root: &Path) -> Result<(), String> {
    let ledger = measure(root)?;
    std::fs::write(root.join(LEDGER_PATH), &ledger)
        .map_err(|e| format!("artsheet: cannot write {LEDGER_PATH}: {e}"))
}

/// Verify the committed ledger against a fresh measurement, and every
/// measurement against its bound.
///
/// # Errors
///
/// A measurement that breaches its bound, a ledger that has drifted, or one
/// that cannot be read.
pub fn check(root: &Path) -> Result<(), String> {
    let produced = measure(root)?;
    let path = root.join(LEDGER_PATH);
    let committed = std::fs::read_to_string(&path)
        .map_err(|e| format!("artsheet: cannot read {LEDGER_PATH}: {e}"))?;
    if committed == produced {
        return Ok(());
    }
    Err(format!(
        "artsheet: {LEDGER_PATH} is out of date; regenerate it with \
         `cargo xtask artsheet --write` and review the diff.\n{}",
        first_difference(&committed, &produced)
    ))
}

/// Render the contact sheets into the gitignored output directory.
///
/// Every figure at every side, including the ones the ledger measures only
/// at the smallest: a sheet is for a human to judge, and a form is judged
/// best where it is drawn largest.
///
/// # Errors
///
/// A measurement that breaches its bound, or a sheet that cannot be
/// written.
pub fn sheets(root: &Path) -> Result<(), String> {
    let out = root.join(SHEETS_DIR);
    std::fs::create_dir_all(&out)
        .map_err(|e| format!("artsheet: cannot create {SHEETS_DIR}: {e}"))?;
    let mut written = Vec::new();
    let shipped = presets(root)?;
    let mut entries = Vec::new();
    for entry in reference::grid() {
        entries.push(drawn(entry)?);
    }
    for (name, identity) in &shipped {
        entries.push(preset_figure(name, identity));
    }
    for entry in &entries {
        let figure = build(entry)?;
        for kind in entry.kinds() {
            for side in SIDES {
                let path = out.join(format!("{}-{}-{side}.png", entry.name, kind.name()));
                std::fs::write(&path, sheet(&figure, *kind, side)?)
                    .map_err(|e| format!("artsheet: cannot write {}: {e}", path.display()))?;
                written.push(path);
            }
        }
    }
    report(&written);
    Ok(())
}

/// Walk the grid and every shipped preset, measure every cell, and render
/// the ledger.
fn measure(root: &Path) -> Result<String, String> {
    undyed_cloth_is_readable()?;
    let mut placement = Placement::new();
    let mut brush = Brush::new();

    let mut ledger = String::with_capacity(1 << 19);
    ledger.push_str(HEADER);
    let _ = writeln!(ledger, "artsheet 2");
    let _ = writeln!(
        ledger,
        "figure-digest {:#018x}",
        figure_digest::REFERENCE_DIGEST
    );
    let _ = writeln!(ledger, "sides {SIDES:?}");

    for entry in reference::grid() {
        let entry = drawn(entry)?;
        let figure = build(&entry)?;
        let rig = figure.rig();
        let rigging = humanoid::rigging(rig).map_err(refused)?;
        let tones = declared(rig);
        let shaded = shades(&tones);
        ledger.push('\n');
        let _ = writeln!(
            ledger,
            "figure {} record {} parts {} tones {} reach {:.6}",
            entry.name,
            record(&entry)?,
            rig.parts().len(),
            tones.len(),
            rig.reach()
        );
        for kind in entry.kinds() {
            motion_row(&mut ledger, &entry, &figure, &rigging, *kind)?;
        }
        for cell in entry.cells() {
            for side in sides(&entry) {
                let measured = cell_row(
                    &entry,
                    &figure,
                    (&tones, &shaded),
                    cell,
                    *side,
                    &mut placement,
                    &mut brush,
                )?;
                ledger.push_str(&measured);
            }
        }
    }
    every_dye_stays_readable(&mut placement, &mut brush)?;

    // The presets a player is offered are bundle content, and each is held
    // to the grid's bounds in every motion at the readability floor. Its
    // record and its motions carry rows; its cells are bounds only, as the
    // dyes' are, since a change to one shows in its record already.
    for (name, identity) in presets(root)? {
        let entry = preset_figure(&name, &identity);
        let figure = build(&entry)?;
        let rig = figure.rig();
        let rigging = humanoid::rigging(rig).map_err(refused)?;
        let tones = declared(rig);
        let shaded = shades(&tones);
        ledger.push('\n');
        let _ = writeln!(
            ledger,
            "preset {} record {} parts {} tones {} reach {:.6}",
            entry.name,
            hex(&identity),
            rig.parts().len(),
            tones.len(),
            rig.reach()
        );
        for kind in entry.kinds() {
            motion_row(&mut ledger, &entry, &figure, &rigging, *kind)?;
        }
        for cell in entry.cells() {
            cell_row(
                &entry,
                &figure,
                (&tones, &shaded),
                cell,
                SIDES[0],
                &mut placement,
                &mut brush,
            )?;
        }
    }
    Ok(ledger)
}

/// Every shipped preset, by name, in name order: each file under
/// [`PRESETS_DIR`] carrying [`RECORD_EXTENSION`], decoded as the record the
/// game will build a figure from.
///
/// Fails closed on a file that is not a record: a preset nobody can build is
/// a packaging defect, not something to skip.
fn presets(root: &Path) -> Result<Vec<(String, Identity)>, String> {
    let dir = root.join(PRESETS_DIR);
    let listing =
        std::fs::read_dir(&dir).map_err(|e| format!("artsheet: cannot list {PRESETS_DIR}: {e}"))?;
    let mut found = Vec::new();
    for item in listing {
        let path = item
            .map_err(|e| format!("artsheet: cannot list {PRESETS_DIR}: {e}"))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(RECORD_EXTENSION) {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("artsheet: {} is not a named preset", path.display()))?
            .to_owned();
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("artsheet: cannot read {}: {e}", path.display()))?;
        let identity = Identity::decode(&bytes)
            .map_err(|e| format!("artsheet: preset {name} is refused: {e}"))?;
        found.push((name, identity));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    if found.is_empty() {
        return Err(format!("artsheet: no presets under {PRESETS_DIR}"));
    }
    Ok(found)
}

/// A preset as a grid entry: every motion, like a species' reference.
fn preset_figure<'a>(name: &'a str, identity: &Identity) -> Figure<'a> {
    Figure {
        name,
        spec: identity.spec(),
        sampling: Sampling::Every,
    }
}

/// Every dye a record can ask for, on each species' palest and darkest
/// build, held at the readability floor to the bounds the grid's own cells
/// are.
///
/// A palette is the player's choice, so what the grid proves of the cloth
/// its figures wear has to hold of every cloth they could. Bounds only: the
/// grid already carries the worst of it, and a row per dye would bury the
/// ledger's diff.
fn every_dye_stays_readable(placement: &mut Placement, brush: &mut Brush) -> Result<(), String> {
    let floor = SIDES[0];
    for entry in reference::FIGURES
        .iter()
        .filter(|entry| matches!(entry.sampling, Sampling::Walk))
    {
        for index in 0..DYES.len() {
            let dye = u8::try_from(index).map_err(|_| "artsheet: a dye past a byte".to_owned())?;
            if dye == entry.spec.palette.accent {
                continue;
            }
            let mut spec = entry.spec;
            spec.palette.accent = dye;
            let dyed = Figure { spec, ..*entry };
            let figure = build(&dyed)?;
            let tones = declared(figure.rig());
            let shaded = shades(&tones);
            for cell in dyed.cells() {
                cell_row(
                    &dyed,
                    &figure,
                    (&tones, &shaded),
                    cell,
                    floor,
                    placement,
                    brush,
                )
                .map_err(|e| format!("{e}, wearing dye {dye}"))?;
            }
        }
    }
    Ok(())
}

/// The one colour every figure wears whatever its palette clears both
/// desktops on its own, which is what makes every palette a record can ask
/// for readable against either.
fn undyed_cloth_is_readable() -> Result<(), String> {
    let lit = mesh::shaded(TROUSERS, LEVELS - 1);
    let dark = contrast(lit, desktop(&Theme::dark()));
    let light = contrast(lit, desktop(&Theme::light()));
    bound("trousers", "contrast-dark", dark, dark >= MIN_CONTRAST)?;
    bound("trousers", "contrast-light", light, light >= MIN_CONTRAST)
}

/// A grid entry, or the generator's refusal to draw one.
fn drawn(
    entry: Result<Figure<'static>, tairix_wintersun_figure::identity::IdentityError>,
) -> Result<Figure<'static>, String> {
    entry.map_err(|e| format!("artsheet: a generated figure is refused: {e}"))
}

/// `entry`'s figure, built from its checked record.
fn build(entry: &Figure<'_>) -> Result<Reference, String> {
    let identity = entry
        .identity()
        .map_err(|e| format!("artsheet: {} is refused: {e}", entry.name))?;
    Reference::new(&identity).map_err(refused)
}

/// `entry`'s record, as the hex a ledger row carries.
fn record(entry: &Figure<'_>) -> Result<String, String> {
    let identity = entry
        .identity()
        .map_err(|e| format!("artsheet: {} is refused: {e}", entry.name))?;
    Ok(hex(&identity))
}

/// A record's bytes as lowercase hex.
fn hex(identity: &Identity) -> String {
    identity
        .encode()
        .iter()
        .fold(String::with_capacity(2 * RECORD_LEN), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The sides `entry` is measured at.
fn sides(entry: &Figure<'_>) -> &'static [u32] {
    match entry.sampling {
        Sampling::Every => &SIDES,
        Sampling::Walk => &SIDES[..1],
    }
}

/// Measure one motion of one figure, and render its ledger row.
///
/// Which measurements a motion is held to, and their bounds, are the figure
/// crate's own ([`Measured`]), so the harness and the crate's tests cannot
/// disagree about either.
fn motion_row(
    ledger: &mut String,
    entry: &Figure<'_>,
    figure: &Reference,
    rigging: &Rigging<'_>,
    kind: Kind,
) -> Result<(), String> {
    let name = format!("{} {}", entry.name, kind.name());
    let clip = figure.clip(kind).map_err(refused)?;
    let legs = figure.legs();
    let measured = Measured::of(kind, rigging, clip, &legs).map_err(refused)?;
    if let Some((what, value)) = measured.breach() {
        bound(&name, what, value, false)?;
    }
    let _ = write!(ledger, "motion {name} seconds {:.6}", clip.seconds());
    for (what, value, _) in measured.each() {
        let _ = write!(ledger, " {what} {value:.6}");
    }
    if let Some(authored) = kind.stride() {
        let _ = write!(ledger, " stride {authored:.6}");
    }
    ledger.push('\n');
    Ok(())
}

/// Measure one cell at one size, and render its ledger row.
///
/// `palette` is the figure's declared tones and every shade of them, in the
/// order [`shades`] lists them.
fn cell_row(
    entry: &Figure<'_>,
    figure: &Reference,
    palette: (&[Color], &[Color]),
    cell: Cell,
    side: u32,
    placement: &mut Placement,
    brush: &mut Brush,
) -> Result<String, String> {
    let (scale, at) = fit(figure, cell.kind, side)?;
    let planted = figure.place(cell, scale, at, placement).map_err(refused)?;
    let miss = planted.worst_miss();
    let name = format!(
        "{} {} {} {:#06x} {side}",
        entry.name,
        cell.kind.name(),
        cell.step,
        cell.facing.0
    );
    bound(&name, "miss", miss, miss <= MAX_MISS)?;

    let (tones, shaded) = palette;
    for strip in placement.strips() {
        if !shaded.contains(&strip.color) {
            return Err(format!(
                "artsheet: {name} paints {:?}, which is no shade of a declared tone",
                strip.color
            ));
        }
    }

    let mut bare = surface(side)?;
    paint::draw(&mut bare, &[], placement, brush, Veil::NONE);
    let mut whole = surface(side)?;
    let shadow = Reference::shadow(0.0, scale, at).map_err(refused)?;
    paint::draw(&mut whole, &[shadow], placement, brush, Veil::NONE);

    let tone = classify(&bare, shaded);
    // Restated at the scale a standing figure is drawn in this cell, so the
    // band reads the same for a motion framed smaller to fit raised arms.
    let (standing, _) = fit(figure, Kind::Idle, side)?;
    let cover = coverage(&bare) * (standing / scale) * (standing / scale);
    let regions = regions(&bare, &tone);
    let shares = shares(&tone, tones.len());
    let (dark, light) = (
        readable(&shares, tones, desktop(&Theme::dark())),
        readable(&shares, tones, desktop(&Theme::light())),
    );
    let cost = paint::cost(placement);
    let overdraw = cost.fill_area / f64::from(side) / f64::from(side);

    bound(
        &name,
        "coverage",
        cover,
        (COVERAGE.0..=COVERAGE.1).contains(&cover),
    )?;
    bound(&name, "contrast-dark", dark, dark >= MIN_CONTRAST)?;
    bound(&name, "contrast-light", light, light >= MIN_CONTRAST)?;
    bound(&name, "overdraw", overdraw, overdraw <= MAX_OVERDRAW)?;
    if regions < MIN_REGIONS {
        return Err(format!(
            "artsheet: {name} resolves into {regions} tonal regions, under {MIN_REGIONS}"
        ));
    }
    if cost.points > MAX_FIGURE_POINTS {
        return Err(format!(
            "artsheet: {name} traces {} outline points, over {MAX_FIGURE_POINTS}",
            cost.points
        ));
    }

    Ok(format!(
        "cell {name}  pixels {:#018x}  coverage {cover:.6}  regions {regions:<2}  \
         contrast-dark {dark:.4}  contrast-light {light:.4}  points {:<4}  \
         overdraw {overdraw:.6}\n",
        pixels(&whole),
        cost.points
    ))
}

/// Where a figure is drawn in `kind` in a square cell of `side` pixels: the
/// stage's own framing for that motion, which the designer's preview shares.
fn fit(figure: &Reference, kind: Kind, side: u32) -> Result<(f64, (f64, f64)), String> {
    reference::fit(kind, figure.rig().reach(), side).map_err(refused)
}

/// One contact sheet: a motion at one size, phases across and headings down.
fn sheet(figure: &Reference, kind: Kind, side: u32) -> Result<Vec<u8>, String> {
    /// Pixels of gutter between cells and around the sheet.
    const GUTTER: u32 = 1;
    /// The neutral the cells are laid on, so a pale figure and a dark one
    /// both read against it.
    const BACKING: Color = Color::rgb(0x7A, 0x7A, 0x7A);

    let columns = u32::try_from(PHASES).unwrap_or(u32::MAX);
    let rows = u32::try_from(FACINGS.len()).unwrap_or(u32::MAX);
    let step = side + GUTTER;
    let mut sheet = Surface::filled(
        columns * step + GUTTER,
        rows * step + GUTTER,
        BACKING.premultiply(),
    )
    .ok_or("artsheet: a sheet that size could not be allocated")?;

    let (scale, at) = fit(figure, kind, side)?;
    let mut placement = Placement::new();
    let mut brush = Brush::new();
    for (row, facing) in FACINGS.into_iter().enumerate() {
        for step_index in 0..PHASES {
            let cell = Cell {
                kind,
                step: step_index,
                facing,
            };
            figure
                .place(cell, scale, at, &mut placement)
                .map_err(refused)?;
            let mut drawn = surface(side)?;
            let shadow = Reference::shadow(0.0, scale, at).map_err(refused)?;
            paint::draw(&mut drawn, &[shadow], &placement, &mut brush, Veil::NONE);
            let x =
                i32::try_from(u32::try_from(step_index).unwrap_or(0) * step + GUTTER).unwrap_or(0);
            let y = i32::try_from(u32::try_from(row).unwrap_or(0) * step + GUTTER).unwrap_or(0);
            sheet.blit(x, y, &drawn);
        }
    }

    let mut rgba = Vec::with_capacity(sheet.pixels().len() * 4);
    for pixel in sheet.pixels() {
        let straight = pixel.unpremultiply();
        rgba.extend_from_slice(&[straight.r, straight.g, straight.b, straight.a]);
    }
    png::encode(sheet.width(), sheet.height(), &rgba)
}

/// A transparent square cell.
fn surface(side: u32) -> Result<Surface, String> {
    Surface::new(side, side).ok_or_else(|| format!("artsheet: no surface at {side}x{side}"))
}

/// The alpha-weighted fraction of the cell the figure covers.
fn coverage(surface: &Surface) -> f64 {
    let total: u64 = surface.pixels().iter().map(|p| u64::from(p.a)).sum();
    #[allow(
        clippy::cast_precision_loss,
        reason = "a cell's alpha sum is far below the mantissa's own range"
    )]
    let ratio = total as f64 / (255.0 * surface.pixels().len() as f64);
    ratio
}

/// The tones `rig` draws in: each role one of its surfaces is drawn in, once.
///
/// Read off the rig rather than listed here, so a figure is held to exactly
/// the colours its own record chose, and a role no surface uses cannot claim
/// pixels in the classification.
fn declared(rig: &Rig) -> Vec<Color> {
    let mut tones = Vec::with_capacity(Tint::COUNT);
    for tint in Tint::ALL {
        if rig.parts().iter().any(|part| part.tint() == tint) {
            let tone = rig.tints().get(tint);
            if !tones.contains(&tone) {
                tones.push(tone);
            }
        }
    }
    tones
}

/// Every pixel's declared tone, if it is one, given every shade of every
/// declared tone in the order [`shades`] lists them.
fn classify(surface: &Surface, shaded: &[Color]) -> Vec<Option<u8>> {
    surface
        .pixels()
        .iter()
        .map(|pixel| shade(*pixel, shaded))
        .collect()
}

/// How much of the figure each declared tone covers.
///
/// Over the pixels the painter left at exactly one of the figure's tones, so
/// an antialiased edge — which is a blend of two of them and of the ground —
/// counts toward neither.
fn shares(tone: &[Option<u8>], count: usize) -> Vec<f64> {
    let mut weight = vec![0u64; count];
    for slot in tone.iter().flatten() {
        if let Some(held) = weight.get_mut(usize::from(*slot)) {
            *held += 1;
        }
    }
    let total: u64 = weight.iter().sum();
    if total == 0 {
        return vec![0.0; count];
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a cell's pixel counts are far below the mantissa's own range"
    )]
    let share = |count: u64| count as f64 / total as f64;
    weight.into_iter().map(share).collect()
}

/// The best contrast any substantial tone of the figure reaches against
/// `background`.
///
/// Taken at the tone's own lit end rather than its base, because what a
/// player sees of a rounded surface is the side facing the light.
fn readable(shares: &[f64], tones: &[Color], background: Color) -> f64 {
    let mut best = 0.0;
    for (tone, share) in tones.iter().zip(shares) {
        if *share >= MIN_TONE_SHARE {
            best = f64::max(best, contrast(mesh::shaded(*tone, LEVELS - 1), background));
        }
    }
    best
}

/// Which declared tone a pixel is, if it is one, given every shade of every
/// declared tone in the order [`shades`] lists them.
///
/// The nearest shade within [`TONE_SLACK`]: the painter composites a
/// straight-alpha colour, so every pixel a single shape covers carries that
/// shape's own shade however partially it covers it, and only where two
/// shapes overlap does the answer become a blend that belongs to neither.
fn shade(pixel: tairix_raster::color::Pixel, shaded: &[Color]) -> Option<u8> {
    if pixel.a < MIN_TONE_ALPHA {
        return None;
    }
    let straight = pixel.unpremultiply();
    let mut nearest = (TONE_SLACK, None);
    for (index, lit) in shaded.iter().enumerate() {
        let apart = channel_gap(lit.r, straight.r)
            + channel_gap(lit.g, straight.g)
            + channel_gap(lit.b, straight.b);
        if apart < nearest.0 {
            nearest = (apart, u8::try_from(index / LEVELS as usize).ok());
        }
    }
    nearest.1
}

/// How far apart two channel readings are, squared.
fn channel_gap(one: u8, other: u8) -> u32 {
    let apart = u32::from(one.abs_diff(other));
    apart * apart
}

/// Every colour a figure may paint in: each declared tone at each step of
/// the shading ladder, tone-major.
///
/// Derived from the figure's own tones and the painter's own ladder rather
/// than listed here, so the conformance check cannot drift from what the
/// painter does.
fn shades(tones: &[Color]) -> Vec<Color> {
    let mut out = Vec::with_capacity(tones.len() * LEVELS as usize);
    for base in tones {
        for level in 0..LEVELS {
            out.push(mesh::shaded(*base, level));
        }
    }
    out
}

/// How many connected regions of one declared tone the cell resolves into,
/// given every pixel's tone.
///
/// Eight-connected, because two pixels meeting at a corner are one mass to
/// the eye and splitting them would measure the connectivity convention
/// rather than the figure.
fn regions(surface: &Surface, tone: &[Option<u8>]) -> u32 {
    let (width, height) = (surface.width(), surface.height());
    let cells = surface.pixels().len();

    let mut seen = vec![false; cells];
    let mut found = 0;
    let mut stack = Vec::new();
    for start in 0..cells {
        let Some(shade) = tone[start] else { continue };
        if seen[start] {
            continue;
        }
        let mut size = 0u32;
        stack.push(start);
        seen[start] = true;
        while let Some(index) = stack.pop() {
            size += 1;
            let here = u32::try_from(index).unwrap_or(0);
            let (x, y) = (here % width, here / width);
            for (dx, dy) in NEIGHBOURS {
                let (Some(nx), Some(ny)) = (x.checked_add_signed(dx), y.checked_add_signed(dy))
                else {
                    continue;
                };
                if nx >= width || ny >= height {
                    continue;
                }
                let next = usize::try_from(ny * width + nx).unwrap_or(cells);
                if next < cells && !seen[next] && tone[next] == Some(shade) {
                    seen[next] = true;
                    stack.push(next);
                }
            }
        }
        if size >= MIN_REGION_PIXELS {
            found += 1;
        }
    }
    found
}

/// The eight pixels touching one.
const NEIGHBOURS: [(i32, i32); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// The theme's desktop backing, which is what a figure is seen against.
fn desktop(theme: &Theme) -> Color {
    let ground = theme.palette().desktop;
    Color::rgb(ground.r, ground.g, ground.b)
}

/// The WCAG contrast ratio between two opaque tones.
pub(super) fn contrast(a: Color, b: Color) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    let (high, low) = if x > y { (x, y) } else { (y, x) };
    (high + 0.05) / (low + 0.05)
}

/// A tone's relative luminance, as the contrast definition states it.
fn luminance(color: Color) -> f64 {
    fn channel(value: u8) -> f64 {
        let linear = f64::from(value) / 255.0;
        if linear <= 0.040_45 {
            linear / 12.92
        } else {
            ((linear + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// A digest of the cell's pixels, which is what a drift check compares.
fn pixels(surface: &Surface) -> u64 {
    use core::hash::Hasher as _;
    let mut hasher = FastHash::with_seed(figure_digest::REFERENCE_SEED);
    hasher.write_u32(surface.width());
    hasher.write_u32(surface.height());
    for pixel in surface.pixels() {
        hasher.write(&[pixel.r, pixel.g, pixel.b, pixel.a]);
    }
    hasher.finish()
}

/// Fail closed on a breached bound, naming the cell and the number.
fn bound(what: &str, name: &str, measured: f64, holds: bool) -> Result<(), String> {
    if holds {
        return Ok(());
    }
    Err(format!(
        "artsheet: {what} {name} is {measured:.6}, outside the bound the art is held to"
    ))
}

/// Name what the figure engine refused.
fn refused(error: tairix_wintersun_figure::FigureError) -> String {
    format!("artsheet: the figure engine refused: {error}")
}

/// The first line the committed ledger and a fresh measurement differ on,
/// so a failure names the change rather than announcing one.
fn first_difference(committed: &str, produced: &str) -> String {
    for (number, (was, now)) in committed.lines().zip(produced.lines()).enumerate() {
        if was != now {
            return format!("  line {}:\n  - {was}\n  + {now}", number + 1);
        }
    }
    format!(
        "  the ledger has {} lines, the measurement {}",
        committed.lines().count(),
        produced.lines().count()
    )
}

/// Say where the sheets went, so a reviewer can go and look at them.
fn report(written: &[PathBuf]) {
    for path in written {
        eprintln!("xtask: [artsheet --sheets] {}", path.display());
    }
}

/// What the committed ledger opens with.
const HEADER: &str = "\
# The figure art ledger: the measured state of WinterSun's figures.
#
# Regenerated by `cargo xtask artsheet --write` and verified by the bare
# `cargo xtask artsheet`, which `ci` runs. Every number here is measured
# rather than authored, and each has a bound beside its measurement — the
# pose-side ones in `tairix_wintersun_figure::quality`, the pixel-side ones
# in `tools/xtask/src/commands/artsheet.rs`. A bound is never widened to
# admit a change; a number that moves is a change to what a figure does.
#
# A `figure` row is one figure of the grid: its name, its record, and how
# many surfaces and tones it is drawn from. A `preset` row is one figure the
# game ships as a preset, measured in every motion at the readability floor
# and held to every bound there; its cells carry no rows of their own.
# A `motion` row is one shipped clip played by it, with each measurement the
# clip is held to by name, and a gait's authored stride. A `cell` row is one
# rendered frame: its figure, its motion, which of the eight phases, the
# heading it faces, the pixel side it was drawn at, a digest of its pixels,
# and its measurements.
#
# The contact sheets themselves are rendered on demand by
# `cargo xtask artsheet --sheets` into the gitignored `images/artsheet/`,
# one per figure, motion and side, phases across and headings down, so the
# picture a reviewer judges is always current rather than
# as-of-last-regeneration.
#
";

#[cfg(test)]
#[path = "artsheet/tests.rs"]
mod tests;
