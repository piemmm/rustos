//! The closed folder taxonomy the program library files applications under.

/// One of the fixed folders the program library organises applications
/// into.
///
/// The set is **closed** and its spellings are locale-neutral identifiers,
/// not display text: a store naming anything else is refused at parse time
/// and a bundle declaring anything else is refused at catalog-write time,
/// so a catalog can never grow a free-form folder. The names follow the
/// well-understood freedesktop.org main-menu categories, so a third-party
/// packager already knows where a bundle lands.
///
/// Presentation — the localised folder label a launcher shows — belongs to
/// the surface that draws the folder, never to this identifier.
///
/// There is deliberately **no settings folder**: system settings are
/// reached from the system overview, not from the program library, so a
/// bundle cannot file itself among the applications a user launches.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum LibraryCategory {
    /// Calculator, text editor, clock, notes, archive tools.
    Accessories,
    /// Image viewers and editors, screenshot tools.
    Graphics,
    /// Browser, mail, chat, remote access.
    Internet,
    /// Audio and video players and recorders.
    Multimedia,
    /// Documents, spreadsheets, PDF.
    Office,
    /// Editors, IDEs, terminals used as development tools, debuggers.
    Programming,
    /// Games.
    Games,
    /// Monitors, disk tools, task shells.
    SystemTools,
    /// Small single-purpose tools that fit no folder above.
    Utilities,
    /// The catch-all a bundle that declares no category lands in.
    #[default]
    Other,
}

impl LibraryCategory {
    /// Every folder, in the order a launcher presents them.
    ///
    /// The order is the declaration order — the curated reading order of
    /// the taxonomy — and is the total order [`Ord`] derives, so a folder
    /// list and a sort of the same folders can never disagree.
    pub const ALL: [Self; 10] = [
        Self::Accessories,
        Self::Graphics,
        Self::Internet,
        Self::Multimedia,
        Self::Office,
        Self::Programming,
        Self::Games,
        Self::SystemTools,
        Self::Utilities,
        Self::Other,
    ];

    /// The canonical, locale-neutral identifier a store spells this folder
    /// with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accessories => "Accessories",
            Self::Graphics => "Graphics",
            Self::Internet => "Internet",
            Self::Multimedia => "Multimedia",
            Self::Office => "Office",
            Self::Programming => "Programming",
            Self::Games => "Games",
            Self::SystemTools => "SystemTools",
            Self::Utilities => "Utilities",
            Self::Other => "Other",
        }
    }

    /// Decode a folder identifier; `None` for anything outside the closed
    /// set.
    ///
    /// The match is exact and case-sensitive: one canonical spelling, so a
    /// rendered store has exactly one valid form and two catalogs cannot
    /// disagree over `games` and `Games`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == id)
    }

    /// The longest folder identifier, in bytes: the widest a folder value
    /// can render to, and the bound the store's line budget is derived
    /// from.
    pub const MAX_ID_LEN: usize = {
        let mut longest = 0;
        let mut index = 0;
        while index < Self::ALL.len() {
            let len = Self::ALL[index].as_str().len();
            if len > longest {
                longest = len;
            }
            index += 1;
        }
        longest
    };
}

impl core::fmt::Display for LibraryCategory {
    /// Renders the canonical identifier [`from_id`](Self::from_id) accepts
    /// back unchanged.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::LibraryCategory;

    #[test]
    fn every_identifier_round_trips_through_the_decoder() {
        for category in LibraryCategory::ALL {
            assert_eq!(LibraryCategory::from_id(category.as_str()), Some(category));
        }
    }

    #[test]
    fn the_folder_list_holds_every_variant_exactly_once() {
        let mut seen = LibraryCategory::ALL;
        seen.sort_unstable();
        for pair in seen.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate folder in ALL");
        }
        assert_eq!(seen.len(), LibraryCategory::ALL.len());
    }

    #[test]
    fn the_presentation_order_is_the_sort_order() {
        let mut sorted = LibraryCategory::ALL;
        sorted.sort_unstable();
        assert_eq!(sorted, LibraryCategory::ALL);
    }

    #[test]
    fn a_folder_outside_the_closed_set_is_refused() {
        for hostile in ["games", "GAMES", "Settings", "", " Games", "Games "] {
            assert_eq!(LibraryCategory::from_id(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn an_undeclared_category_lands_in_the_catch_all_folder() {
        assert_eq!(LibraryCategory::default(), LibraryCategory::Other);
    }

    #[test]
    fn the_widest_identifier_bounds_every_spelling() {
        assert!(LibraryCategory::ALL
            .into_iter()
            .all(|category| category.as_str().len() <= LibraryCategory::MAX_ID_LEN));
        assert!(LibraryCategory::ALL
            .into_iter()
            .any(|category| category.as_str().len() == LibraryCategory::MAX_ID_LEN));
    }

    #[test]
    fn the_rendered_folder_is_the_identifier() {
        for category in LibraryCategory::ALL {
            assert_eq!(alloc::format!("{category}"), category.as_str());
        }
    }
}
