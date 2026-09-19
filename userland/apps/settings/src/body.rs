//! What the pane on show draws in the content column.
//!
//! Three shapes, and the registry decides which from the pane's own row: a
//! stated absence, a form of settables, or read-only readings discovered at
//! runtime. The Wallpaper pane is the one that carries two things at once —
//! its rows fixed at the top of the column with the shipped pictures
//! scrolling beneath them — and it is its own variant rather than a pair of
//! options, so "a gallery with no form" is a state the shell cannot be in.
//!
//! The shell asks this what to measure, what to draw, and how it scrolls;
//! it never asks which of two options happens to be set.

use tairix_geometry::{Rect, Scale};
use tairix_icon::IconArtwork;
use tairix_raster::Surface;
use tairix_sysconfig::SystemConfig;
use tairix_theme::{CursorSetId, Theme};
use tairix_wallpaper::{CatalogItem, DesktopSettings};

use crate::facts::{Facts, MachineFacts, NetworkFacts};
use crate::form::{Documents, Form, FormPlace};
use crate::gallery::Gallery;
use crate::registry::{PaneContent, PaneRow};
use crate::statement;
use crate::volumes::{Readings, VolumeReading};

/// Everything a body is built from: what the shell has been answered so
/// far, and the document every settable row reads.
///
/// A pane opens on whatever has arrived — nothing at all, at first — and is
/// rebuilt when the rest lands, so none of these is awaited.
pub(crate) struct Answered<'a> {
    /// The desktop's own settings document.
    pub(crate) settings: &'a DesktopSettings,
    /// The cursor sets the desktop answered with.
    pub(crate) cursor_sets: &'a [CursorSetId],
    /// The shipped pictures the desktop answered with.
    pub(crate) catalog: &'a [CatalogItem],
    /// The mounted volumes the mount-table walk answered with.
    pub(crate) volumes: &'a [VolumeReading],
    /// The machine's boot-time configuration, or `None` while the read has
    /// not landed. Absent is not the same fact as a store of defaults, so
    /// a machine row with no reading says so rather than showing one.
    pub(crate) config: Option<&'a SystemConfig>,
    /// The machine readings the caller took for the panes that state them.
    pub(crate) machine: &'a MachineFacts,
    /// The network readings the caller took for the panes that state them.
    pub(crate) network: &'a NetworkFacts,
}

impl<'a> Answered<'a> {
    /// The stores a form's rows are built from.
    const fn documents(&self) -> Documents<'a> {
        Documents {
            settings: self.settings,
            cursor_sets: self.cursor_sets,
            config: self.config,
        }
    }
}

/// The content column's contents.
pub(crate) enum Body {
    /// The pane composes no controls and says why: what this system does
    /// not have, or where the setting is reached instead.
    Statement,
    /// A form of settables over the desktop's settings document.
    Form(Form),
    /// The Wallpaper pane: its form fixed at the top of the column, with
    /// the picture gallery scrolling beneath it.
    Pictures {
        /// The settable rows, which stay put.
        form: Form,
        /// The pictures, which are what scrolls.
        gallery: Gallery,
    },
    /// The mounted volumes: one read-only card each, discovered rather than
    /// declared.
    Volumes(Readings),
    /// A read-only column of machine readings: what this machine is, or
    /// what its clock says.
    Facts(Facts),
}

impl Body {
    /// What `pane` draws, from what the shell has been answered.
    pub(crate) fn of(pane: &PaneRow, answered: &Answered<'_>) -> Self {
        match pane.content() {
            None => Self::Statement,
            Some(PaneContent::Form(composition)) => {
                Self::Form(Form::new(composition, answered.documents()))
            }
            Some(PaneContent::Pictures(composition)) => Self::Pictures {
                form: Form::new(composition, answered.documents()),
                gallery: Gallery::new(answered.catalog, answered.settings),
            },
            Some(PaneContent::Volumes) => Self::Volumes(Readings::new(answered.volumes)),
            Some(PaneContent::About) => Self::Facts(Facts::about(answered.machine)),
            Some(PaneContent::Clock) => Self::Facts(Facts::clock(answered.machine)),
            Some(PaneContent::Dns) => Self::Facts(Facts::resolvers(answered.network)),
        }
    }

    /// The form this body composes, if it composes one.
    pub(crate) const fn form(&self) -> Option<&Form> {
        match self {
            Self::Form(form) | Self::Pictures { form, .. } => Some(form),
            Self::Statement | Self::Volumes(_) | Self::Facts(_) => None,
        }
    }

    /// The form this body composes, to route an event into.
    pub(crate) const fn form_mut(&mut self) -> Option<&mut Form> {
        match self {
            Self::Form(form) | Self::Pictures { form, .. } => Some(form),
            Self::Statement | Self::Volumes(_) | Self::Facts(_) => None,
        }
    }

    /// The picture gallery this body draws, if it draws one.
    pub(crate) const fn gallery(&self) -> Option<&Gallery> {
        match self {
            Self::Pictures { gallery, .. } => Some(gallery),
            Self::Statement | Self::Form(_) | Self::Volumes(_) | Self::Facts(_) => None,
        }
    }

    /// The picture gallery this body draws, to route an event into.
    pub(crate) const fn gallery_mut(&mut self) -> Option<&mut Gallery> {
        match self {
            Self::Pictures { gallery, .. } => Some(gallery),
            Self::Statement | Self::Form(_) | Self::Volumes(_) | Self::Facts(_) => None,
        }
    }

    /// Whether this body composes controls a reader can act on.
    ///
    /// What decides whether the pane column is on the focus ring in its own
    /// right: a body with controls is reachable whether or not it is long
    /// enough to scroll, while one that only scrolls is reachable exactly
    /// when there is something to scroll.
    pub(crate) const fn composes_controls(&self) -> bool {
        self.form().is_some()
    }

    /// Whether the column is scrolled by *pixels* rather than by whole
    /// plates or tile lines.
    ///
    /// Only a statement is: it is clipped to the column, so it can be drawn
    /// at any offset. A plate is *placed* on the surface instead — one given
    /// a negative top draws nothing and hit-tests as nothing — so a body of
    /// plates scrolls a whole plate at a time.
    pub(crate) const fn scrolls_in_pixels(&self) -> bool {
        matches!(self, Self::Statement)
    }

    /// Whether this body stages a change to the machine's store, and so
    /// needs that store read before its rows can show anything.
    pub(crate) fn stages_machine_settings(&self) -> bool {
        self.form()
            .is_some_and(|form| form.posture() == crate::form::Posture::Staged)
    }

    /// Whether a choice list is open, which is modal: the list keeps the
    /// pointer even where it hangs outside the pane's own column.
    pub(crate) fn is_listing(&self) -> bool {
        self.form().is_some_and(Form::is_listing)
    }

    /// The height the body itself needs in a column `width` pixels wide.
    ///
    /// A gallery's own extent is not counted: it is the band left beneath a
    /// form that stays put, measured where that band is resolved.
    pub(crate) fn measured_height(
        &self,
        pane: &PaneRow,
        width: u32,
        scale: Scale,
        theme: &Theme,
    ) -> u32 {
        match self {
            Self::Statement => statement::measured_height(pane, width, scale, theme),
            Self::Form(form) | Self::Pictures { form, .. } => form.measured_height(scale, theme),
            Self::Volumes(readings) => readings.measured_height(scale, theme),
            Self::Facts(facts) => facts.measured_height(scale, theme),
        }
    }

    /// Draw from plate `index`, for a body that scrolls by whole plates.
    pub(crate) fn set_first(&mut self, index: usize) {
        match self {
            Self::Form(form) => form.set_first(index),
            Self::Volumes(readings) => readings.set_first(index),
            Self::Facts(facts) => facts.set_first(index),
            // A statement is clipped rather than placed, and a gallery's
            // own offset is the scroll model's: neither draws *from* a
            // plate.
            Self::Statement | Self::Pictures { .. } => {}
        }
    }

    /// The scroll extent and how much of it the column shows, in the unit
    /// this body scrolls by.
    ///
    /// `band` is what the column has left beneath a fixed form, resolved by
    /// the shell because only it knows the frame.
    pub(crate) fn scroll_range(
        &self,
        pane: &PaneRow,
        place: FormPlace<'_>,
        band: Rect,
        offset: u64,
    ) -> (u64, u64) {
        match self {
            Self::Statement => (
                u64::from(self.measured_height(pane, place.bounds.width, place.scale, place.theme)),
                u64::from(place.bounds.height),
            ),
            Self::Form(form) => (
                crate::stack::as_extent(form.groups_len()),
                crate::stack::as_extent(form.seated(place)),
            ),
            Self::Pictures { gallery, .. } => {
                let range = gallery.scroll_range(band, place.scale, place.theme, offset);
                (range.content_extent(), range.viewport_extent())
            }
            Self::Volumes(readings) => (
                crate::stack::as_extent(readings.len()),
                crate::stack::as_extent(readings.seated(place.bounds, place.scale, place.theme)),
            ),
            Self::Facts(facts) => (
                crate::stack::as_extent(Facts::len()),
                crate::stack::as_extent(facts.seated(place.bounds, place.scale, place.theme)),
            ),
        }
    }

    /// Adopt the desktop settings the session now holds.
    pub(crate) fn adopt(&mut self, settings: &DesktopSettings) {
        match self {
            Self::Form(form) => form.adopt(settings),
            Self::Pictures { form, gallery } => {
                form.adopt(settings);
                gallery.adopt(settings);
            }
            // None reads the desktop's document: one states the registry's
            // own words, the others the machine's volumes and readings.
            Self::Statement | Self::Volumes(_) | Self::Facts(_) => {}
        }
    }

    /// Draw the body into `surface`.
    ///
    /// `column` is the pane's own rectangle, which rides above the frame
    /// while a pixel-scrolled statement is scrolled; `band` is what is left
    /// beneath a fixed form for the gallery.
    pub(crate) fn render(
        &self,
        surface: &mut Surface,
        pane: &PaneRow,
        drawn: Drawn<'_>,
        artwork: &mut dyn IconArtwork,
    ) {
        let Drawn {
            place,
            column,
            band,
            offset,
        } = drawn;
        match self {
            Self::Statement => statement::render(surface, pane, column, place.scale, place.theme),
            Self::Form(form) => form.render(surface, place),
            Self::Pictures { form, gallery } => {
                form.render(surface, place);
                gallery.render(surface, band, offset, place.scale, place.theme);
            }
            Self::Volumes(readings) => {
                readings.render(surface, place.bounds, place.scale, place.theme, artwork);
            }
            Self::Facts(facts) => facts.render(surface, place.bounds, place.scale, place.theme),
        }
    }
}

/// Where a body is drawn, gathered for the one call that draws it.
#[derive(Copy, Clone)]
pub(crate) struct Drawn<'a> {
    /// The pane column a form's plates stack down, and the surface every
    /// length is resolved against.
    pub(crate) place: FormPlace<'a>,
    /// The pane's own rectangle, which a pixel-scrolled statement rides
    /// above the frame in.
    pub(crate) column: Rect,
    /// The band left beneath a fixed form, where a gallery is drawn.
    pub(crate) band: Rect,
    /// How far that band is scrolled, in tile lines.
    pub(crate) offset: u64,
}
