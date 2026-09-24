//! The Storage pane: one card per mounted volume, read-only.
//!
//! Unlike every other composed pane, this one has no fixed table of
//! settables behind it — its cards are the volumes the machine turns out to
//! have, so they are built from the mount table at runtime and there is
//! nothing to write. Mounting and unmounting are the file manager's and
//! `mount`'s; a second route to them here would be two ways to do one thing.
//!
//! Every figure is derived by the one shared volume view model
//! (`tairix_procinfo::volume`), which the Switchboard's storage page and
//! `df` read too, so a volume cannot be half full on one surface and nearly
//! full on another. Nothing is derived here.
//!
//! A volume whose format tracks no capacity gets no track and no invented
//! percentage: its card says so in the row where the reading would have
//! been.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::sysinfo::{MountAvailability, MountRecord};
use tairix_controls::{
    stack, FieldControl, FieldGroup, FieldLayout, FieldRow, MeterValue, MetricInstrument,
    MetricTile, PressureKind, ProgressValue, StatusPill,
};
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_icon::{disk_icon, IconArtwork, IconRequest};
use tairix_procinfo::{
    availability_name, medium_name, mount_name_bytes, volume_health_name, VolumeBytes,
};
use tairix_raster::Surface;
use tairix_theme::{SignalRole, Theme};
use tairix_util::size::{format_binary, SIZE_TEXT_MAX};

/// The label of the capacity reading: the tile's own, and the row that
/// states its absence.
const CAPACITY: &str = "Capacity";
/// The label of the mount-point row.
const MOUNTED_AT: &str = "Mounted at";
/// The label of the filesystem row.
const FILESYSTEM: &str = "Filesystem";
/// The label of the backing-device row.
const DEVICE: &str = "Device";
/// The label of the storage-medium row.
const MEDIUM: &str = "Medium";
/// The label of the availability row, spelled so it cannot be read as the
/// available *bytes* the capacity card reports.
const AVAILABILITY: &str = "Availability";

/// Every label a storage card draws, which is the pane's whole contribution
/// to the search index.
///
/// The registry's row reads this rather than restating it, so a term that
/// reaches this pane always reaches a row it actually shows.
pub(crate) const VOLUME_FACTS: &[&str] = &[
    CAPACITY,
    MOUNTED_AT,
    FILESYSTEM,
    DEVICE,
    MEDIUM,
    AVAILABILITY,
];

/// What a field states when the mount table reports nothing for it.
const NOT_REPORTED: &str = "not reported";

/// One mounted volume, as this pane reads it.
///
/// Decoded from the wire record at the edge of the program, so the pane
/// itself holds owned text and the shell never touches a transport. The
/// decode is here rather than in the `Run` binary because the host build
/// compiles that binary's stub and never its body, so logic living there is
/// logic nothing tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeReading {
    /// What the volume is called: its backing source, or its mount point
    /// where the table gives it no source.
    name: String,
    /// Where it is mounted.
    target: String,
    /// The driver serving it.
    fstype: String,
    /// Its backing device, which may be nothing at all for a synthetic
    /// mount.
    source: String,
    /// The medium it sits on, or `None` where the mount has no block
    /// backing or the table did not classify it.
    medium: Option<BlkDeviceClass>,
    /// Whether it is still answering.
    availability: MountAvailability,
    /// What it holds, or `None` where the format tracks no fixed capacity.
    bytes: Option<VolumeBytes>,
}

impl VolumeReading {
    /// What `record` says about its volume.
    #[must_use]
    pub fn of(record: &MountRecord) -> Self {
        Self {
            name: text(mount_name_bytes(record)),
            target: text(record.target_bytes()),
            fstype: text(record.fstype_bytes()),
            source: text(record.source_bytes()),
            medium: record.medium(),
            availability: record.availability(),
            bytes: VolumeBytes::of(&record.usage()),
        }
    }

    /// What the volume is called.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A field's text, or the stated absence of one.
///
/// A blank slot would read as a reading of nothing, which is exactly the
/// fabrication this surface exists to avoid.
fn reading(text: &str) -> FieldControl {
    if text.is_empty() {
        return FieldControl::Unmeasured(String::from(NOT_REPORTED));
    }
    FieldControl::Reading(String::from(text))
}

/// Wire bytes as display text, lossily: a driver that reported a name this
/// build cannot decode still names something, rather than vanishing.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// One volume's card: the plate of facts it is identified by, and the
/// capacity reading beneath it.
///
/// Both plates are one scrolled unit, so a volume's capacity can never be
/// on screen without the volume it belongs to.
struct VolumeCard {
    /// The volume's facts, captioned with its name and badged with its
    /// banded health.
    facts: FieldGroup,
    /// Its capacity, or `None` where the format tracks none — in which case
    /// the facts carry the stated absence instead of a bar of nothing.
    capacity: Option<MetricTile>,
}

impl VolumeCard {
    /// The card `volume` draws.
    fn of(volume: &VolumeReading) -> Self {
        let health = volume.availability.health();
        let mut rows = alloc::vec![
            FieldRow::new(MOUNTED_AT, reading(&volume.target)),
            FieldRow::new(FILESYSTEM, reading(&volume.fstype)),
            FieldRow::new(DEVICE, reading(&volume.source)),
            FieldRow::new(MEDIUM, reading(medium_name(volume.medium))),
            FieldRow::new(
                AVAILABILITY,
                reading(availability_name(volume.availability))
            ),
        ];
        if volume.bytes.is_none() {
            rows.push(FieldRow::new(
                CAPACITY,
                FieldControl::Unmeasured(String::from("this format tracks no fixed capacity")),
            ));
        }
        Self {
            facts: FieldGroup::new(volume.name.clone(), rows).with_badge(
                StatusPill::new(volume_health_name(health))
                    .with_tone(SignalRole::for_volume_health(health)),
            ),
            capacity: volume
                .bytes
                .map(|bytes| capacity_tile(bytes, volume.medium)),
        }
    }

    /// The height the whole card needs at `width`, which its facts' wrapped
    /// descriptions depend on.
    fn measured_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let column = self.facts.slot_column(width, scale, theme);
        let facts = self.facts.measured_height(width, column, scale, theme);
        match &self.capacity {
            Some(tile) => facts
                .saturating_add(stack::gap(scale, theme))
                .saturating_add(tile.measured_height(scale, theme)),
            None => facts,
        }
    }

    /// Draw the card into `surface` filling `bounds`.
    fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: &mut dyn IconArtwork,
    ) {
        let column = self.facts.slot_column(bounds.width, scale, theme);
        let facts_h = self
            .facts
            .measured_height(bounds.width, column, scale, theme);
        let plate = Rect::new(bounds.left(), bounds.top(), bounds.width, facts_h);
        self.facts
            .render(surface, FieldLayout::new(plate, column), scale, theme);
        let Some(tile) = &self.capacity else {
            return;
        };
        let below = facts_h.saturating_add(stack::gap(scale, theme));
        let Some(height) = bounds.height.checked_sub(below) else {
            return;
        };
        let rect = Rect::new(
            bounds.left(),
            bounds.top().saturating_add(to_i32(below)),
            bounds.width,
            height,
        );
        // Resolved at the side the tile actually draws it at, so what the
        // cache holds is what appears.
        let picture = tile.icon().and_then(|kind| {
            artwork.artwork(IconRequest::kind(kind), tile.icon_side(rect, scale, theme))
        });
        tile.render(surface, rect, scale, theme, picture);
    }
}

/// The capacity card for a volume whose format reports one: how much of the
/// medium is gone, what is left, and the track showing the share.
///
/// The share is of the whole medium, which is what a capacity bar means — a
/// `df`-style `Use%` asks a different question and divides by what a caller
/// may allocate, so the two are never each other's default.
fn capacity_tile(bytes: VolumeBytes, medium: Option<BlkDeviceClass>) -> MetricTile {
    let mut used = [0u8; SIZE_TEXT_MAX];
    let mut total = [0u8; SIZE_TEXT_MAX];
    let mut available = [0u8; SIZE_TEXT_MAX];
    let value = alloc::format!(
        "{} of {}",
        format_binary(bytes.used(), &mut used),
        format_binary(bytes.total, &mut total)
    );
    let track = MeterValue::Measured(ProgressValue::new(bytes.used_permille()));
    MetricTile::new(CAPACITY, value, PressureKind::Disk)
        .with_icon(disk_icon(medium))
        .with_detail(alloc::format!(
            "{} available",
            format_binary(bytes.available, &mut available)
        ))
        .with_instrument(MetricInstrument::Track(track))
}

/// The Storage pane's body: one card per mounted volume, and which of them
/// the column draws from.
///
/// Read-only throughout, so it routes no input: the pane's keyboard is its
/// scrollbar's, exactly as a pane that states an absence.
pub struct Readings {
    cards: Vec<VolumeCard>,
    /// The first card drawn. A card is *placed* on the surface rather than
    /// clipped to it, so the column scrolls by whole cards.
    first: usize,
}

impl Readings {
    /// The cards `volumes` draws.
    #[must_use]
    pub fn new(volumes: &[VolumeReading]) -> Self {
        Self {
            cards: volumes.iter().map(VolumeCard::of).collect(),
            first: 0,
        }
    }

    /// How many volumes are shown.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cards.len()
    }

    /// Whether the machine reported no mounted volume at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// The first card drawn.
    #[must_use]
    pub const fn first(&self) -> usize {
        self.first
    }

    /// Draw from card `index`, clamped to the last.
    pub fn set_first(&mut self, index: usize) {
        self.first = index.min(self.cards.len().saturating_sub(1));
    }

    /// The physical height every card needs, stacked in a column `width`
    /// pixels wide.
    #[must_use]
    pub fn measured_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let plate = stack::plate_width(width, scale, theme);
        stack::height(
            self.cards
                .iter()
                .map(|card| card.measured_height(plate, scale, theme)),
            scale,
            theme,
        )
    }

    /// How many cards the column seats from the one it draws from.
    #[must_use]
    pub fn seated(&self, bounds: Rect, scale: Scale, theme: &Theme) -> usize {
        self.placed(bounds, scale, theme).len()
    }

    /// Draw the cards into `surface` stacked down `bounds`.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: &mut dyn IconArtwork,
    ) {
        for (index, rect) in self.placed(bounds, scale, theme) {
            if let Some(card) = self.cards.get(index) {
                card.render(surface, rect, scale, theme, artwork);
            }
        }
    }

    /// Where each drawn card is placed.
    fn placed(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<(usize, Rect)> {
        stack::place(
            bounds,
            self.first,
            self.cards.len(),
            scale,
            theme,
            |index| {
                let plate = stack::plate_width(bounds.width, scale, theme);
                self.cards
                    .get(index)
                    .map_or(0, |card| card.measured_height(plate, scale, theme))
            },
        )
    }

    /// The caption of card `index`, for a test that asks what a volume's
    /// plate is headed with.
    #[cfg(test)]
    pub(crate) fn caption(&self, index: usize) -> Option<&str> {
        Some(self.cards.get(index)?.facts.caption())
    }

    /// The rows of card `index`, for a test that asks what a volume states.
    #[cfg(test)]
    pub(crate) fn rows(&self, index: usize) -> Option<&[FieldRow]> {
        Some(self.cards.get(index)?.facts.rows())
    }

    /// Card `index`'s health capsule.
    #[cfg(test)]
    pub(crate) fn pill(&self, index: usize) -> Option<&StatusPill> {
        self.cards.get(index)?.facts.badge()
    }

    /// Card `index`'s capacity tile, or `None` where the volume reports no
    /// capacity.
    #[cfg(test)]
    pub(crate) fn capacity(&self, index: usize) -> Option<&MetricTile> {
        self.cards.get(index)?.capacity.as_ref()
    }
}
