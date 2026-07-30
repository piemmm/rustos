//! Unit tests for the catalog container, the user overlay patch, and the
//! machine ∪ overlay merge.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::LibraryCategory;

use super::{merge, Catalog, CatalogFull, EntryPatch, Record, MAX_ENTRIES};
use crate::entry::{BundlePath, DisplayName, EntryId, IconAsset, LibraryEntry};

/// An entry filed under `category`, keyed and named after `leaf`.
fn entry(leaf: &str, category: LibraryCategory) -> LibraryEntry {
    LibraryEntry::new(
        id(leaf),
        name(leaf),
        BundlePath::new(&format!("/Apps/{leaf}.app")).expect("bundle path"),
        category,
        None,
    )
}

/// The identifier [`entry`] keys `leaf` under.
fn id(leaf: &str) -> EntryId {
    EntryId::new(&format!("org.tairix.{leaf}")).expect("entry id")
}

fn name(text: &str) -> DisplayName {
    DisplayName::new(text).expect("display name")
}

fn icon(file: &str) -> IconAsset {
    IconAsset::new(file).expect("icon asset")
}

fn listed_names(listed: &[&LibraryEntry]) -> Vec<String> {
    listed
        .iter()
        .map(|entry| String::from(entry.name().as_str()))
        .collect()
}

/// A catalog holding one declared entry per `leaf`.
fn declaring(leaves: &[(&str, LibraryCategory)]) -> Catalog {
    let mut catalog = Catalog::new();
    for &(leaf, category) in leaves {
        catalog
            .insert(entry(leaf, category))
            .expect("within the record bound");
    }
    catalog
}

/// A catalog holding `patch` against `leaf`'s identifier.
fn patching(leaf: &str, patch: EntryPatch) -> Catalog {
    let mut catalog = Catalog::new();
    catalog.patch(id(leaf), patch).expect("patch recorded");
    catalog
}

#[test]
fn reconcile_declares_only_identifiers_no_record_claims() {
    // A curated entry (re-filed by an administrator) and a patch both
    // stand; only the genuinely new bundle is declared.
    let mut catalog = declaring(&[("Editor", LibraryCategory::Office)]);
    let mut hide = EntryPatch::new();
    hide.set_hidden(true);
    catalog.patch(id("Legacy"), hide.clone()).expect("patch");

    let discovered = [
        entry("Editor", LibraryCategory::Utilities),
        entry("Legacy", LibraryCategory::Utilities),
        entry("Terminal", LibraryCategory::Programming),
    ];
    let added = catalog.reconcile(&discovered).expect("within bound");
    assert_eq!(added, 1, "only the unclaimed identifier is declared");
    assert_eq!(
        catalog.entry(&id("Editor")).map(LibraryEntry::category),
        Some(LibraryCategory::Office),
        "the curated entry is untouched"
    );
    assert_eq!(
        catalog.entry_patch(&id("Legacy")),
        Some(&hide),
        "the standing patch is untouched"
    );
    assert_eq!(
        catalog.entry(&id("Terminal")),
        Some(&entry("Terminal", LibraryCategory::Programming))
    );

    // A second identical fold changes nothing: rescan is idempotent.
    assert_eq!(catalog.reconcile(&discovered), Ok(0));
}

#[test]
fn reconcile_keeps_the_first_of_duplicate_discovered_identifiers() {
    let mut catalog = Catalog::new();
    let added = catalog
        .reconcile(&[
            entry("Editor", LibraryCategory::Office),
            entry("Editor", LibraryCategory::Games),
        ])
        .expect("within bound");
    assert_eq!(added, 1);
    assert_eq!(
        catalog.entry(&id("Editor")).map(LibraryEntry::category),
        Some(LibraryCategory::Office),
        "the first discovered entry wins"
    );
}

#[test]
fn reconcile_refuses_the_whole_fold_at_the_record_bound() {
    let mut catalog = Catalog::new();
    for index in 0..MAX_ENTRIES - 1 {
        catalog
            .insert(entry(&format!("app{index}"), LibraryCategory::Other))
            .expect("within bound");
    }
    let discovered = [
        entry("one-more", LibraryCategory::Other),
        entry("one-too-many", LibraryCategory::Other),
    ];
    assert_eq!(catalog.reconcile(&discovered), Err(CatalogFull));
    assert_eq!(
        catalog.len(),
        MAX_ENTRIES - 1,
        "a refused fold leaves the catalog unchanged"
    );
    assert_eq!(catalog.entry(&id("one-more")), None, "never half-applied");

    // Exactly filling the bound is fine.
    assert_eq!(
        catalog.reconcile(&discovered[..1]),
        Ok(1),
        "the bound itself is reachable"
    );
}

#[test]
fn a_fresh_catalog_holds_nothing() {
    let catalog = Catalog::new();
    assert!(catalog.is_empty());
    assert_eq!(catalog.len(), 0);
    assert_eq!(catalog.records().count(), 0);
    assert!(catalog.folders().is_empty());
}

#[test]
fn a_declared_entry_is_read_back_under_its_identifier() {
    let catalog = declaring(&[("Editor", LibraryCategory::Office)]);
    assert_eq!(catalog.len(), 1);
    assert_eq!(
        catalog.entry(&id("Editor")),
        Some(&entry("Editor", LibraryCategory::Office))
    );
    assert_eq!(
        catalog.get(&id("Editor")),
        Some(&Record::Entry(entry("Editor", LibraryCategory::Office)))
    );
    assert_eq!(
        catalog.entry_patch(&id("Editor")),
        None,
        "an identifier holding an entry holds no patch"
    );
    assert_eq!(catalog.entry(&id("Absent")), None);
}

#[test]
fn re_declaring_an_identifier_returns_the_record_it_held() {
    let mut catalog = declaring(&[("Editor", LibraryCategory::Office)]);
    let displaced = catalog
        .insert(entry("Editor", LibraryCategory::Programming))
        .expect("re-declaration");
    assert_eq!(
        displaced,
        Some(Record::Entry(entry("Editor", LibraryCategory::Office)))
    );
    assert_eq!(catalog.len(), 1, "an identifier holds exactly one record");
    assert_eq!(
        catalog.entry(&id("Editor")).map(LibraryEntry::category),
        Some(LibraryCategory::Programming)
    );
}

#[test]
fn a_patch_and_an_entry_displace_one_another_under_one_identifier() {
    let mut catalog = declaring(&[("Editor", LibraryCategory::Office)]);

    let mut hide = EntryPatch::new();
    hide.set_hidden(true);
    assert_eq!(
        catalog.patch(id("Editor"), hide.clone()),
        Ok(Some(Record::Entry(entry(
            "Editor",
            LibraryCategory::Office
        ))))
    );
    assert_eq!(catalog.entry(&id("Editor")), None);
    assert_eq!(catalog.entry_patch(&id("Editor")), Some(&hide));

    assert_eq!(
        catalog.insert(entry("Editor", LibraryCategory::Office)),
        Ok(Some(Record::Patch(hide)))
    );
    assert_eq!(catalog.entry_patch(&id("Editor")), None);
}

#[test]
fn recording_a_patch_that_changes_nothing_clears_the_personalisation() {
    let mut renamed = EntryPatch::new();
    renamed.set_name(name("Text Editor"));
    let mut catalog = patching("Editor", renamed.clone());

    assert_eq!(
        catalog.patch(id("Editor"), EntryPatch::new()),
        Ok(Some(Record::Patch(renamed))),
        "an empty patch clears what the identifier held"
    );
    assert!(
        catalog.is_empty(),
        "and leaves no unrenderable record behind"
    );
    assert_eq!(
        catalog.patch(id("Editor"), EntryPatch::new()),
        Ok(None),
        "clearing what was never held is not an error"
    );
}

#[test]
fn an_empty_patch_reports_itself_empty_until_a_field_is_set() {
    assert!(EntryPatch::new().is_empty());
    let setters: [fn(&mut EntryPatch); 4] = [
        |patch| patch.set_name(name("Renamed")),
        |patch| patch.set_category(LibraryCategory::Games),
        |patch| patch.set_icon(icon("chess.svg")),
        |patch| patch.set_hidden(false),
    ];
    for set in setters {
        let mut patch = EntryPatch::new();
        set(&mut patch);
        assert!(!patch.is_empty(), "a set field is a change");
    }
}

#[test]
fn a_patch_reads_back_every_field_it_was_given() {
    let mut patch = EntryPatch::new();
    assert_eq!(patch.name(), None);
    assert_eq!(patch.category(), None);
    assert_eq!(patch.icon(), None);
    assert_eq!(patch.hidden(), None);

    patch.set_name(name("Text Editor"));
    patch.set_category(LibraryCategory::Utilities);
    patch.set_icon(icon("editor.svg"));
    patch.set_hidden(true);
    assert_eq!(patch.name(), Some(&name("Text Editor")));
    assert_eq!(patch.category(), Some(LibraryCategory::Utilities));
    assert_eq!(patch.icon(), Some(&icon("editor.svg")));
    assert_eq!(patch.hidden(), Some(true));
}

#[test]
fn removing_a_record_returns_it_once() {
    let mut catalog = declaring(&[("Editor", LibraryCategory::Office)]);
    assert_eq!(
        catalog.remove(&id("Editor")),
        Some(Record::Entry(entry("Editor", LibraryCategory::Office)))
    );
    assert_eq!(catalog.remove(&id("Editor")), None);
    assert!(catalog.is_empty());
}

#[test]
fn the_record_bound_fails_closed_and_still_admits_a_replacement() {
    let mut catalog = Catalog::new();
    for index in 0..MAX_ENTRIES {
        catalog
            .insert(entry(&format!("App{index}"), LibraryCategory::Other))
            .expect("within the record bound");
    }
    assert_eq!(catalog.len(), MAX_ENTRIES);

    let mut hide = EntryPatch::new();
    hide.set_hidden(true);
    assert_eq!(
        catalog.insert(entry("OneTooMany", LibraryCategory::Other)),
        Err(CatalogFull)
    );
    assert_eq!(
        catalog.patch(id("OneTooMany"), hide.clone()),
        Err(CatalogFull)
    );
    assert_eq!(catalog.len(), MAX_ENTRIES, "a refused record does not land");

    assert!(
        catalog.patch(id("App0"), hide).is_ok(),
        "an identifier already held is replaceable at the bound"
    );
    assert!(catalog
        .insert(entry("App1", LibraryCategory::Games))
        .is_ok());
    assert_eq!(catalog.len(), MAX_ENTRIES);
}

#[test]
fn the_full_catalog_refusal_says_what_was_wrong() {
    assert_eq!(
        format!("{CatalogFull}"),
        "catalog already holds the maximum number of records"
    );
}

#[test]
fn records_entries_and_patches_are_iterated_in_identifier_order() {
    let mut catalog = declaring(&[
        ("Zebra", LibraryCategory::Other),
        ("Apple", LibraryCategory::Other),
        ("Mango", LibraryCategory::Other),
    ]);
    let mut refiled = EntryPatch::new();
    refiled.set_category(LibraryCategory::Games);
    catalog
        .patch(id("Beetle"), refiled)
        .expect("patch recorded");

    let ids: Vec<&str> = catalog.records().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "org.tairix.Apple",
            "org.tairix.Beetle",
            "org.tairix.Mango",
            "org.tairix.Zebra",
        ]
    );
    assert_eq!(
        listed_names(&catalog.entries().collect::<Vec<_>>()),
        ["Apple", "Mango", "Zebra"]
    );
    let patched: Vec<&str> = catalog.patches().map(|(id, _)| id.as_str()).collect();
    assert_eq!(patched, ["org.tairix.Beetle"]);
}

#[test]
fn a_folder_lists_its_own_entries_by_name() {
    let catalog = declaring(&[
        ("Chess", LibraryCategory::Games),
        ("Backgammon", LibraryCategory::Games),
        ("Editor", LibraryCategory::Office),
    ]);
    assert_eq!(
        listed_names(&catalog.folder(LibraryCategory::Games)),
        ["Backgammon", "Chess"]
    );
    assert_eq!(
        listed_names(&catalog.folder(LibraryCategory::Office)),
        ["Editor"]
    );
    assert!(catalog.folder(LibraryCategory::Internet).is_empty());
}

#[test]
fn two_identically_named_applications_keep_a_stable_order() {
    let mut catalog = declaring(&[("Chess", LibraryCategory::Games)]);
    let mut namesake = entry("Zulu", LibraryCategory::Games);
    namesake.set_name(name("Chess"));
    catalog.insert(namesake).expect("declaration");

    let listed = catalog.folder(LibraryCategory::Games);
    assert_eq!(listed_names(&listed), ["Chess", "Chess"]);
    let ids: Vec<&str> = listed.iter().map(|entry| entry.id().as_str()).collect();
    assert_eq!(ids, ["org.tairix.Chess", "org.tairix.Zulu"]);
}

#[test]
fn only_folders_that_hold_an_entry_are_offered_and_in_taxonomy_order() {
    let mut catalog = declaring(&[
        ("Chess", LibraryCategory::Games),
        ("Editor", LibraryCategory::Office),
    ]);
    let mut refiled = EntryPatch::new();
    refiled.set_category(LibraryCategory::Internet);
    catalog
        .patch(id("Absent"), refiled)
        .expect("patch recorded");

    assert_eq!(
        catalog.folders(),
        [LibraryCategory::Office, LibraryCategory::Games],
        "presentation order, and no folder a patch alone names"
    );
}

#[test]
fn merging_two_empty_catalogs_yields_an_empty_catalog() {
    assert_eq!(merge(&Catalog::new(), &Catalog::new()), Catalog::new());
}

#[test]
fn an_overlay_entry_shadows_the_machine_entry_of_the_same_identifier() {
    let machine = declaring(&[("Editor", LibraryCategory::Office)]);
    let mut mine = entry("Editor", LibraryCategory::Programming);
    mine.set_name(name("My Editor"));
    let mut overlay = Catalog::new();
    overlay.insert(mine.clone()).expect("declaration");

    let resolved = merge(&machine, &overlay);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved.entry(&id("Editor")), Some(&mine));
}

#[test]
fn a_machine_wide_patch_adjusts_the_entry_it_names() {
    let machine = declaring(&[("Editor", LibraryCategory::Office)]);
    let mut wide = EntryPatch::new();
    wide.set_name(name("Machine Editor"));
    wide.set_category(LibraryCategory::Utilities);
    wide.set_icon(icon("machine.svg"));

    let resolved = merge(&machine, &patching("Editor", wide));
    let editor = resolved.entry(&id("Editor")).expect("entry survives");
    assert_eq!(editor.name().as_str(), "Machine Editor");
    assert_eq!(editor.category(), LibraryCategory::Utilities);
    assert_eq!(editor.icon(), Some(&icon("machine.svg")));
    assert_eq!(
        editor.bundle(),
        entry("Editor", LibraryCategory::Office).bundle(),
        "a patch never re-points the bundle it launches"
    );
}

#[test]
fn the_users_own_patch_wins_field_by_field_over_the_machine_wide_one() {
    let machine = declaring(&[("Editor", LibraryCategory::Office)]);
    let mut wide = EntryPatch::new();
    wide.set_name(name("Machine Editor"));
    wide.set_category(LibraryCategory::Utilities);
    wide.set_icon(icon("machine.svg"));
    let adjusted = merge(&machine, &patching("Editor", wide));

    let mut mine = EntryPatch::new();
    mine.set_name(name("My Editor"));
    let resolved = merge(&adjusted, &patching("Editor", mine));

    let editor = resolved.entry(&id("Editor")).expect("entry survives");
    assert_eq!(
        editor.name().as_str(),
        "My Editor",
        "the user's rename wins"
    );
    assert_eq!(
        editor.category(),
        LibraryCategory::Utilities,
        "a field the user did not touch keeps the machine-wide adjustment"
    );
    assert_eq!(editor.icon(), Some(&icon("machine.svg")));
}

#[test]
fn an_entry_a_patch_hides_is_dropped_from_the_resolved_catalog() {
    let machine = declaring(&[
        ("Editor", LibraryCategory::Office),
        ("Chess", LibraryCategory::Games),
    ]);
    let mut hide = EntryPatch::new();
    hide.set_hidden(true);

    let resolved = merge(&machine, &patching("Editor", hide));
    assert_eq!(resolved.entry(&id("Editor")), None);
    assert!(resolved.entry(&id("Chess")).is_some());
}

#[test]
fn an_overlay_re_shows_what_the_machine_store_declared_hidden() {
    let mut machine = Catalog::new();
    let mut suppressed = entry("Editor", LibraryCategory::Office);
    suppressed.set_hidden(true);
    machine.insert(suppressed).expect("declared");
    assert!(merge(&machine, &Catalog::new()).is_empty());

    let mut show = EntryPatch::new();
    show.set_hidden(false);
    let resolved = merge(&machine, &patching("Editor", show));
    let editor = resolved
        .entry(&id("Editor"))
        .expect("the user's re-show wins");
    assert!(!editor.hidden());
}

#[test]
fn a_hidden_declaration_keeps_its_identifier_claimed_against_reconcile() {
    let mut catalog = Catalog::new();
    let mut suppressed = entry("Editor", LibraryCategory::Office);
    suppressed.set_hidden(true);
    catalog.insert(suppressed.clone()).expect("declared");

    // A rescan re-discovering the same bundle must not resurrect what the
    // curator suppressed: the hidden record already claims the identifier.
    let added = catalog
        .reconcile(&[entry("Editor", LibraryCategory::Office)])
        .expect("within bounds");
    assert_eq!(added, 0);
    assert_eq!(catalog.entry(&id("Editor")), Some(&suppressed));
    assert!(merge(&catalog, &Catalog::new()).is_empty());
}

#[test]
fn a_users_own_hide_takes_effect_over_a_machine_wide_show() {
    let machine = declaring(&[("Editor", LibraryCategory::Office)]);
    let mut show = EntryPatch::new();
    show.set_hidden(false);
    let shown = merge(&machine, &patching("Editor", show));
    assert_eq!(shown.len(), 1);

    let mut hide = EntryPatch::new();
    hide.set_hidden(true);
    assert!(merge(&shown, &patching("Editor", hide)).is_empty());
}

#[test]
fn a_patch_naming_no_entry_is_discarded_rather_than_fabricating_one() {
    let mut orphan = EntryPatch::new();
    orphan.set_name(name("Uninstalled"));
    orphan.set_hidden(true);
    let overlay = patching("Uninstalled", orphan.clone());

    assert!(merge(&Catalog::new(), &overlay).is_empty());
    assert_eq!(
        overlay.entry_patch(&id("Uninstalled")),
        Some(&orphan),
        "the personalisation stays in the user's own document"
    );
}

#[test]
fn the_resolved_catalog_declares_entries_only() {
    let machine = declaring(&[("Editor", LibraryCategory::Office)]);
    let mut refiled = EntryPatch::new();
    refiled.set_category(LibraryCategory::Games);

    let resolved = merge(&machine, &patching("Editor", refiled));
    assert_eq!(resolved.patches().count(), 0);
    assert_eq!(resolved.entries().count(), 1);
    assert_eq!(
        resolved.entry(&id("Editor")).map(LibraryEntry::category),
        Some(LibraryCategory::Games)
    );
}

#[test]
fn merging_a_resolved_catalog_again_changes_nothing() {
    let machine = declaring(&[
        ("Editor", LibraryCategory::Office),
        ("Chess", LibraryCategory::Games),
    ]);
    let resolved = merge(&machine, &Catalog::new());
    assert_eq!(merge(&resolved, &Catalog::new()), resolved);
    assert_eq!(resolved, machine);
}
