//! Client tests over a fake host: the layered read, the capacity
//! negotiation, what a `set` does and does not stage, and how a degraded
//! store behaves.
//!
//! The fake host answers the `appdata-v1` wire exactly as the service does —
//! it decodes real request frames and encodes real reply frames — so these
//! tests exercise the codec the client and the daemon actually share, not a
//! mock of it.

use alloc::string::String;

use tairix_abi::appdata_ipc::{APPDATA_DOCUMENT_MAX, APPDATA_MAX_REPLY};
use tairix_abi::Errno;
use tairix_appconf::ConfError;

use super::fake::FakeService;
use super::{Settings, READ_ATTEMPTS};

/// The word the fake bundle is installed under.
const OWN_WORD: &str = "notes";

/// The bundle directory the fake resolves for [`OWN_WORD`].
const BUNDLE: &str = "/System/Applications/notes.app";

/// A fake service for [`OWN_WORD`] with a bundle that ships no defaults.
fn service() -> FakeService {
    FakeService::for_word(OWN_WORD).with_bundle(BUNDLE)
}

/// A fake service whose bundle ships `text` as its defaults.
fn service_with_defaults(text: &str) -> FakeService {
    FakeService::for_word(OWN_WORD).with_defaults(BUNDLE, text)
}

#[test]
fn a_read_answers_from_the_highest_layer_that_sets_the_key() {
    let mut host =
        service_with_defaults("scheme = light\nfont.size = 14\n").with_store("scheme = dark\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(settings.get("scheme"), Some("dark"), "the store wins");
    assert_eq!(
        settings.get("font.size"),
        Some("14"),
        "and the shipped default fills what it does not set"
    );
    assert_eq!(settings.get("nothing"), None);
    assert_eq!(settings.store_refusal(), None);
    assert_eq!(settings.defaults_refusal(), None);
}

#[test]
fn opening_costs_one_call_and_every_read_after_it_costs_none() {
    let mut host = service().with_store("a = 1\nb = 2\nc = 3\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    for key in ["a", "b", "c", "d"] {
        let _ = settings.get(key);
    }
    drop(settings);
    assert_eq!(host.calls(), 1, "a settings read is not one call per key");
}

#[test]
fn typed_reads_distinguish_absent_from_valid_from_malformed() {
    let mut host = service().with_store("on = true\nsize = 14\nblur = 500\nbad = nope\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(settings.bool("on"), Ok(Some(true)));
    assert_eq!(settings.u32("size"), Ok(Some(14)));
    assert_eq!(settings.permille("blur"), Ok(Some(500)));
    assert_eq!(settings.i64("size"), Ok(Some(14)));
    assert_eq!(settings.bool("missing"), Ok(None));
    assert_eq!(settings.u32("bad"), Err(ConfError::ValueMalformed));
    assert_eq!(settings.bool("bad"), Err(ConfError::ValueMalformed));
}

#[test]
fn a_set_and_commit_publishes_and_reads_back() {
    let mut host = service();
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.set_u32("font.size", 16).expect("a legal setting");
    assert!(settings.is_dirty());
    assert_eq!(
        settings.u32("font.size"),
        Ok(Some(16)),
        "a handle reads back its own unpublished edit"
    );
    settings.commit().expect("publishes");
    assert!(!settings.is_dirty());
    assert_eq!(settings.u32("font.size"), Ok(Some(16)));
    drop(settings);
    assert_eq!(host.committed().get("font.size"), Some("16"));
}

#[test]
fn nothing_is_staged_for_a_value_the_layers_already_answer_with() {
    // An application that saves a setting it did not change must not rewrite
    // the user's document, and a value that already comes from the defaults
    // layer must not be copied up into it.
    let mut host = service_with_defaults("scheme = light\n").with_store("font.size = 14\n");
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.set("scheme", "light").expect("legal");
    settings.set_u32("font.size", 14).expect("legal");
    assert!(!settings.is_dirty(), "neither value changed anything");
    settings.commit().expect("a commit with nothing to publish");
    drop(settings);
    assert_eq!(host.calls(), 1, "the commit issued no call at all");
    assert_eq!(
        host.committed().render(),
        "font.size = 14\n",
        "and the user's document is untouched"
    );
}

#[test]
fn a_set_that_differs_from_a_lower_layer_lands_in_the_store() {
    let mut host = service_with_defaults("scheme = light\n");
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.set("scheme", "dark").expect("legal");
    settings.commit().expect("publishes");
    drop(settings);
    assert_eq!(host.committed().render(), "scheme = dark\n");
}

#[test]
fn an_unset_uncovers_the_layer_beneath() {
    let mut host = service_with_defaults("scheme = light\n").with_store("scheme = dark\n");
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.unset("scheme");
    assert!(settings.is_dirty());
    settings.commit().expect("publishes");
    assert_eq!(
        settings.get("scheme"),
        Some("light"),
        "the shipped default applies again"
    );
    drop(settings);
    assert_eq!(host.committed().get("scheme"), None);
}

#[test]
fn unsetting_a_key_no_store_layer_carries_stages_nothing() {
    let mut host = service_with_defaults("scheme = light\n");
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.unset("scheme");
    settings.unset("never.set");
    assert!(
        !settings.is_dirty(),
        "there is nothing in the store to remove"
    );
    drop(settings);
    assert_eq!(host.calls(), 1);
}

#[test]
fn one_commit_publishes_every_edit_and_touches_each_key_once() {
    let mut host = service().with_store("a = 1\n");
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.set("a", "2").expect("legal");
    settings.set("a", "3").expect("legal");
    settings.set_bool("b", true).expect("legal");
    settings.commit().expect("publishes");
    drop(settings);
    // One read at open, one set per edited key, one commit, one read after.
    assert_eq!(host.calls(), 5);
    assert_eq!(host.committed().render(), "a = 3\nb = true\n");
}

#[test]
fn a_malformed_key_or_value_is_refused_where_the_mistake_was_made() {
    let mut host = service();
    let mut settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(settings.set("Scheme", "dark"), Err(ConfError::KeyInvalid));
    assert_eq!(
        settings.set("scheme", "da\u{7}rk"),
        Err(ConfError::ValueInvalid)
    );
    assert_eq!(
        settings.set_permille("blur", tairix_appconf::PERMILLE_FULL + 1),
        Err(ConfError::ValueMalformed)
    );
    assert!(!settings.is_dirty(), "a refusal stages nothing");
}

#[test]
fn a_failed_commit_leaves_the_edits_for_a_retry() {
    let mut host = service();
    let refuse = host.refusal();
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.set("scheme", "dark").expect("legal");
    refuse.set(Some(Errno::DeviceOffline));
    assert_eq!(settings.commit(), Err(Errno::DeviceOffline));
    assert!(settings.is_dirty(), "the edit survives the failure");
    refuse.set(None);
    settings.commit().expect("the retry lands");
    drop(settings);
    assert_eq!(host.committed().get("scheme"), Some("dark"));
}

#[test]
fn an_unreachable_service_degrades_to_the_shipped_defaults() {
    // No service yet, a volume still to be unlocked, a caller running no
    // signed bundle: the application still runs, on what its bundle ships,
    // and can say why.
    let mut host = service_with_defaults("scheme = light\nfont.size = 14\n");
    host.refusal().set(Some(Errno::NotFound));
    let mut settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(settings.store_refusal(), Some(Errno::NotFound));
    assert_eq!(settings.get("scheme"), Some("light"));
    assert_eq!(settings.u32("font.size"), Ok(Some(14)));

    // A write fails with the same typed error rather than going nowhere
    // quietly.
    settings.set("scheme", "dark").expect("staged locally");
    assert_eq!(settings.commit(), Err(Errno::NotFound));
    assert!(settings.is_dirty());
}

#[test]
fn a_bundle_shipping_no_defaults_is_the_ordinary_case() {
    let mut host = service().with_store("scheme = dark\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(
        settings.defaults_refusal(),
        None,
        "an absent defaults document is not a defect"
    );
    assert_eq!(settings.get("scheme"), Some("dark"));
}

#[test]
fn a_broken_defaults_document_is_reported_and_leaves_the_layer_empty() {
    let oversize: String = core::iter::repeat_n('x', APPDATA_DOCUMENT_MAX + 1).collect();
    let mut host = service_with_defaults(&oversize);
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(
        settings.defaults_refusal(),
        Some(Errno::LengthOutOfRange),
        "a packaging defect is said out loud"
    );
    assert_eq!(settings.get("scheme"), None);
}

#[test]
fn the_first_candidate_bundle_that_ships_defaults_wins() {
    let mut host = FakeService::for_word(OWN_WORD)
        .with_bundle("/System/Commands/notes.app")
        .with_defaults(BUNDLE, "scheme = light\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(
        settings.get("scheme"),
        Some("light"),
        "a candidate that ships none is skipped, not treated as broken"
    );
}

#[test]
fn a_word_that_names_no_bundle_simply_has_no_defaults_layer() {
    let mut host = service().with_store("scheme = dark\n");
    let settings = Settings::open(&mut host, "not-this-program");
    assert_eq!(settings.defaults_refusal(), None);
    assert_eq!(settings.get("scheme"), Some("dark"));
}

#[test]
fn a_store_larger_than_the_probe_is_read_whole_on_the_second_call() {
    // The first call asks for the probe size and is told the length; the
    // second asks for exactly that and gets the whole document. A prefix is
    // never parsed.
    let mut text = String::new();
    let mut expected = 0usize;
    while text.len() <= super::READ_PROBE {
        let _ = core::fmt::Write::write_fmt(
            &mut text,
            format_args!(
                "recent.{expected} = /Users/ada/Documents/a-reasonably-long-file-name.txt\n"
            ),
        );
        expected += 1;
    }
    let mut host = service().with_store(&text);
    let settings = Settings::open(&mut host, OWN_WORD);
    assert!(settings.get("recent.0").is_some());
    assert!(
        settings
            .get(&alloc::format!("recent.{}", expected - 1))
            .is_some(),
        "the last setting of an oversize store is present, so nothing was truncated"
    );
    drop(settings);
    assert_eq!(host.calls(), 2, "one probe, one exact-size read");
}

#[test]
fn a_writer_that_grows_the_document_under_every_attempt_is_not_chased_for_ever() {
    let mut text = String::new();
    while text.len() <= super::READ_PROBE {
        let index = text.len();
        let _ =
            core::fmt::Write::write_fmt(&mut text, format_args!("k{index} = {}\n", "v".repeat(40)));
    }
    let mut host = service().with_store(&text).with_growing_writer();
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(
        settings.store_refusal(),
        Some(Errno::Busy),
        "a bounded read gives up rather than answering with a partial store"
    );
    drop(settings);
    assert_eq!(host.calls(), READ_ATTEMPTS);
}

#[test]
fn a_reload_discards_unpublished_edits() {
    let mut host = service().with_store("scheme = dark\n");
    let mut settings = Settings::open(&mut host, OWN_WORD);
    settings.set("scheme", "light").expect("legal");
    settings.reload().expect("re-reads");
    assert!(!settings.is_dirty());
    assert_eq!(
        settings.get("scheme"),
        Some("dark"),
        "a handle that never published changed nothing"
    );
}

#[test]
fn the_reply_bound_covers_the_widest_document_the_client_can_ask_for() {
    // The client sizes its own reply buffer, and the endpoint is created with
    // the wire's own bound: a document the format accepts must always fit.
    const { assert!(APPDATA_MAX_REPLY > APPDATA_DOCUMENT_MAX) };
    assert_eq!(APPDATA_DOCUMENT_MAX, tairix_appconf::MAX_DOCUMENT_LEN);
}
