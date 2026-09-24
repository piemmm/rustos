//! The Notifications pane's source plate: one row per program that has posted
//! a notice, and the level each may reach the desktop at.
//!
//! A source is listed once the desktop says it has notified, or once the
//! policy holds a level for it, so a source quietened in an earlier session
//! stays reachable. A source that has never notified is not listed, and a
//! plate with none says so: an empty list reads as *none*, which is the truth.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::BundleId;
use tairix_controls::{ComboBox, FieldControl, FieldGroup, FieldRow};
use tairix_wallpaper::{NotifyLevel, NotifyPolicy};

/// What the source plate contributes to the search index.
pub(crate) const SOURCE_FACTS: &[&str] = &["Notification sources"];

const CAPTION: &str = "SOURCES";

const DESCRIPTION: &str = "The least a notice from this program must matter to be shown.";

/// The label of each level, in [`NotifyLevel::ALL`] order.
const fn level_label(level: NotifyLevel) -> &'static str {
    match level {
        NotifyLevel::All => "All notifications",
        NotifyLevel::Warnings => "Warnings and critical",
        NotifyLevel::Critical => "Critical only",
        NotifyLevel::None => "None",
    }
}

/// The level at `index` of the choices a source row offers.
pub(crate) fn level_at(index: usize) -> Option<NotifyLevel> {
    NotifyLevel::ALL.get(index).copied()
}

/// The source plate for `policy`, given the sources the desktop said have
/// notified (`None` when it did not say), and the source each row sets.
///
/// `full` states that the last change was refused because the policy could
/// hold no more sources.
pub(crate) fn source_group(
    policy: &NotifyPolicy,
    seen: Option<&[BundleId]>,
    full: bool,
) -> (FieldGroup, Vec<BundleId>) {
    let sources = listed(policy, seen.unwrap_or_default());
    let rows: Vec<FieldRow> = if sources.is_empty() {
        alloc::vec![
            FieldRow::new("Sources", FieldControl::Reading(String::from("None")),)
                .with_description("A program is listed here once it has posted a notification.")
        ]
    } else {
        sources
            .iter()
            .map(|source| source_row(source, policy.level_of(source.as_str())))
            .collect()
    };
    let mut group = FieldGroup::new(CAPTION, rows);
    if full {
        group = group.with_footnote(
            "The desktop cannot remember a setting for any more programs. Set one back to All \
             notifications first.",
        );
    } else if seen.is_none() {
        group = group.with_footnote(
            "The desktop did not say which programs have notified, so only those with a setting \
             of their own are listed.",
        );
    }
    (group, sources)
}

/// The sources the plate lists: those the desktop has seen and those the
/// policy holds, each once, in identity order.
fn listed(policy: &NotifyPolicy, seen: &[BundleId]) -> Vec<BundleId> {
    let mut sources: Vec<BundleId> = seen.to_vec();
    sources.extend(
        policy
            .sources()
            .filter_map(|(source, _)| BundleId::new(source).ok()),
    );
    sources.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    sources.dedup_by(|a, b| a.as_str() == b.as_str());
    sources
}

fn source_row(source: &BundleId, level: NotifyLevel) -> FieldRow {
    let mut combo = ComboBox::new(
        NotifyLevel::ALL
            .iter()
            .map(|choice| level_label(*choice).to_string())
            .collect(),
    );
    combo.set_selected(
        NotifyLevel::ALL
            .iter()
            .position(|choice| *choice == level)
            .unwrap_or(0),
    );
    FieldRow::new(source.as_str(), FieldControl::Combo(combo)).with_description(DESCRIPTION)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use tairix_abi::BundleId;
    use tairix_controls::{FieldControl, FieldGroup, FieldRow};
    use tairix_wallpaper::{NotifyLevel, NotifyPolicy};

    use super::{level_at, source_group};

    fn id(text: &str) -> BundleId {
        BundleId::new(text).expect("a bounded identity")
    }

    fn labels(group: &FieldGroup) -> Vec<&str> {
        group.rows().iter().map(FieldRow::label).collect()
    }

    #[test]
    fn a_plate_with_no_source_says_none() {
        let (group, sources) = source_group(&NotifyPolicy::default(), Some(&[]), false);
        assert!(sources.is_empty());
        assert_eq!(labels(&group), ["Sources"]);
        assert!(matches!(
            group.rows()[0].control(),
            FieldControl::Reading(text) if text == "None"
        ));
        assert_eq!(group.footnote(), None);
    }

    #[test]
    fn seen_and_quietened_sources_are_listed_once_in_identity_order() {
        let mut policy = NotifyPolicy::default();
        assert!(policy
            .set_level(id("com.example.mail"), NotifyLevel::None)
            .is_ok());
        assert!(policy
            .set_level(id("com.example.chat"), NotifyLevel::Critical)
            .is_ok());
        let seen = [id("os.tairix.netstack"), id("com.example.chat")];
        let (group, sources) = source_group(&policy, Some(&seen), false);
        assert_eq!(
            labels(&group),
            ["com.example.chat", "com.example.mail", "os.tairix.netstack"]
        );
        assert_eq!(sources.len(), 3);
        let selected: Vec<Option<usize>> = group
            .rows()
            .iter()
            .map(|row| match row.control() {
                FieldControl::Combo(combo) => combo.selected(),
                _ => None,
            })
            .collect();
        assert_eq!(
            selected,
            [Some(2), Some(3), Some(0)],
            "each row shows its source's level"
        );
    }

    #[test]
    fn an_unanswered_desktop_lists_the_policy_and_says_why() {
        let mut policy = NotifyPolicy::default();
        assert!(policy
            .set_level(id("com.example.chat"), NotifyLevel::None)
            .is_ok());
        let (group, _) = source_group(&policy, None, false);
        assert_eq!(labels(&group), ["com.example.chat"]);
        assert!(group.footnote().is_some());
    }

    #[test]
    fn a_full_policy_is_stated() {
        let (group, _) = source_group(&NotifyPolicy::default(), Some(&[]), true);
        assert!(group
            .footnote()
            .is_some_and(|note| note.contains("any more programs")));
    }

    #[test]
    fn every_choice_names_a_level_and_none_past_them() {
        for (index, level) in NotifyLevel::ALL.into_iter().enumerate() {
            assert_eq!(level_at(index), Some(level));
        }
        assert_eq!(level_at(NotifyLevel::ALL.len()), None);
    }
}
