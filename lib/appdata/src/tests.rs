//! Client tests over a fake host: the layered read, the capacity
//! negotiation, what a `set` does and does not stage, and how a degraded
//! store behaves.
//!
//! The fake host answers the `appdata-v1` wire exactly as the service does —
//! it decodes real request frames and encodes real reply frames — so these
//! tests exercise the codec the client and the daemon actually share, not a
//! mock of it.

use alloc::string::String;

use tairix_abi::appdata_ipc::{ConfigScope, APPDATA_DOCUMENT_MAX, APPDATA_MAX_REPLY};
use tairix_abi::Errno;
use tairix_appconf::ConfError;

use super::fake::FakeService;
use super::{read_published, Settings, Vault, READ_ATTEMPTS};

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

#[test]
fn the_published_scope_is_a_document_of_its_own() {
    // What an application says about itself and what the user has set for it
    // are separate documents, and a handle on one cannot see the other.
    let mut host = service()
        .with_store("imap.user = ada\n")
        .with_published("font.family = berkeley\n");

    let private = Settings::open(&mut host, OWN_WORD);
    assert_eq!(private.scope(), ConfigScope::Private);
    assert_eq!(private.get("imap.user"), Some("ada"));
    assert_eq!(private.get("font.family"), None);
    drop(private);

    let mine = Settings::open_published(&mut host);
    assert_eq!(mine.scope(), ConfigScope::Public);
    assert_eq!(mine.get("font.family"), Some("berkeley"));
    assert_eq!(mine.get("imap.user"), None);
}

#[test]
fn publishing_lands_in_the_published_scope_and_nowhere_else() {
    let mut host = service().with_store("imap.user = ada\n");
    let mut mine = Settings::open_published(&mut host);
    mine.set("font.family", "berkeley").expect("legal");
    mine.commit().expect("publishes");
    drop(mine);
    assert_eq!(host.published().render(), "font.family = berkeley\n");
    assert_eq!(
        host.committed().render(),
        "imap.user = ada\n",
        "the private document is untouched"
    );
}

#[test]
fn the_published_scope_has_no_bundle_shipped_layer() {
    // A bundle's shipped defaults are the *private* scope's fallback. The
    // published scope has no layer beneath it — the service cannot name a
    // bundle, so a shipped published document could never be read by anyone
    // else, and what an app publishes is exactly what it wrote.
    let mut host = service_with_defaults("font.family = shipped\n");
    let mine = Settings::open_published(&mut host);
    assert_eq!(mine.get("font.family"), None);
    assert_eq!(mine.defaults_refusal(), None);
    drop(host);

    let mut host = service_with_defaults("font.family = shipped\n");
    let private = Settings::open(&mut host, OWN_WORD);
    assert_eq!(
        private.get("font.family"),
        Some("shipped"),
        "the private scope still reads the shipped layer"
    );
}

#[test]
fn opening_the_published_scope_names_no_bundle_and_costs_one_call() {
    // No command word, because there is no layer to resolve one for — and no
    // bundle read either.
    let mut host = service_with_defaults("font.family = shipped\n").with_published("a = 1\n");
    let mine = Settings::open_published(&mut host);
    for key in ["a", "b", "c"] {
        let _ = mine.get(key);
    }
    drop(mine);
    assert_eq!(host.calls(), 1);
}

#[test]
fn an_unpublishable_store_leaves_the_scope_empty_and_says_why() {
    let mut host = service();
    host.refusal().set(Some(Errno::NotFound));
    let mut mine = Settings::open_published(&mut host);
    assert_eq!(mine.store_refusal(), Some(Errno::NotFound));
    assert_eq!(mine.get("font.family"), None);
    mine.set("font.family", "berkeley").expect("staged locally");
    assert_eq!(mine.commit(), Err(Errno::NotFound));
    assert!(mine.is_dirty(), "so a retry can publish it");
}

#[test]
fn a_foreign_read_answers_what_that_application_publishes() {
    let mut host = service().with_foreign("os.tairix.terminal", "font.family = berkeley\n");
    let theirs = read_published(&mut host, "os.tairix.terminal").expect("reads");
    assert_eq!(theirs.get("font.family"), Some("berkeley"));
    // The typed accessors are the format engine's own, so a foreign read needs
    // no accessor of its own to be useful.
    let mut host = service().with_foreign("os.tairix.terminal", "font.size = 14\n");
    assert_eq!(
        read_published(&mut host, "os.tairix.terminal")
            .expect("reads")
            .u32("font.size"),
        Ok(Some(14))
    );
}

#[test]
fn a_foreign_read_of_an_application_that_publishes_nothing_is_an_empty_document() {
    let mut host = service().with_foreign("os.tairix.terminal", "font.family = berkeley\n");
    let theirs = read_published(&mut host, "com.example.never-run").expect("reads");
    assert!(theirs.settings().next().is_none());
    assert_eq!(theirs.get("font.family"), None);
}

#[test]
fn a_foreign_read_reports_an_unreachable_service_rather_than_answering_empty() {
    // A caller must be able to tell "that app publishes nothing" from "no
    // store can be read at all", because only the second is worth retrying.
    let mut host = service().with_foreign("os.tairix.terminal", "font.family = berkeley\n");
    host.refusal().set(Some(Errno::DeviceOffline));
    assert_eq!(
        read_published(&mut host, "os.tairix.terminal").err(),
        Some(Errno::DeviceOffline)
    );
}

#[test]
fn a_foreign_read_refuses_an_identifier_outside_the_grammar_before_the_call() {
    // The identifier becomes a path component in the store tree, so the wire
    // codec applies the one grammar and the frame never reaches the service.
    let mut host = service();
    for hostile in ["..", "a/b", "OS.tairix", "", ".hidden"] {
        assert!(
            read_published(&mut host, hostile).is_err(),
            "`{hostile}` must never be asked for"
        );
    }
}

#[test]
fn a_published_document_larger_than_the_probe_is_read_whole() {
    // The capacity negotiation is the same one the private scope uses, so a
    // foreign read never parses a prefix either.
    let mut text = String::new();
    let mut count = 0usize;
    while text.len() <= super::READ_PROBE {
        let _ = core::fmt::Write::write_fmt(
            &mut text,
            format_args!("mime.{count} = application/a-reasonably-long-media-type\n"),
        );
        count += 1;
    }
    let mut host = service().with_foreign("os.tairix.terminal", &text);
    let theirs = read_published(&mut host, "os.tairix.terminal").expect("reads");
    assert_eq!(theirs.settings().count(), count);
    assert_eq!(host.calls(), 2, "one probe, one exact-size read");
}

// --- The sealed scope ----------------------------------------------------

#[test]
fn opening_the_sealed_scope_costs_one_call_and_every_read_after_it_costs_none() {
    let mut host = service().with_sealed("imap.password = hunter2\ntoken = abc\n");
    let vault = Vault::open(&mut host).expect("opens");
    assert_eq!(vault.get("imap.password"), Some("hunter2"));
    assert_eq!(vault.get("token"), Some("abc"));
    assert!(vault.has("token"));
    assert_eq!(vault.get("absent"), None);
    assert!(!vault.has("absent"));
    drop(vault);
    assert_eq!(host.calls(), 1, "one round trip, however many secrets");
}

/// The sealed scope has no commit: a write is one call the service applies
/// before it replies, so an application that seals a secret and exits has
/// sealed it.
#[test]
fn a_sealed_write_lands_without_a_commit() {
    let mut host = service();
    let mut vault = Vault::open(&mut host).expect("opens");
    vault.set("imap.password", "hunter2").expect("seals");
    assert_eq!(vault.get("imap.password"), Some("hunter2"));
    drop(vault);
    assert_eq!(host.sealed().get("imap.password"), Some("hunter2"));
}

#[test]
fn sealing_a_secret_the_vault_already_holds_costs_no_call() {
    let mut host = service().with_sealed("imap.password = hunter2\n");
    let mut vault = Vault::open(&mut host).expect("opens");
    vault
        .set("imap.password", "hunter2")
        .expect("seals nothing");
    drop(vault);
    assert_eq!(host.calls(), 1, "only the open");
}

#[test]
fn removing_a_secret_the_vault_does_not_hold_costs_no_call() {
    let mut host = service().with_sealed("token = abc\n");
    let mut vault = Vault::open(&mut host).expect("opens");
    vault.unset("imap.password").expect("removes nothing");
    drop(vault);
    assert_eq!(host.calls(), 1, "only the open");
}

/// The sealed scope has no layer beneath it, so a removal leaves the key
/// absent rather than uncovering a default or a policy value.
#[test]
fn removing_a_secret_uncovers_nothing() {
    let mut host = service_with_defaults("imap.password = a shipped secret\n")
        .with_sealed("imap.password = hunter2\n");
    let mut vault = Vault::open(&mut host).expect("opens");
    vault.unset("imap.password").expect("removes");
    assert_eq!(vault.get("imap.password"), None);
    drop(vault);
    assert_eq!(host.sealed().get("imap.password"), None);
}

/// The one place the sealed scope deliberately differs from
/// [`Settings::open`]: it fails rather than reading empty, because "I could not
/// read your secrets" is not "you have none".
#[test]
fn an_unreadable_vault_is_reported_and_never_reads_as_empty() {
    for err in [
        Errno::SignatureInvalid,
        Errno::BadMagic,
        Errno::DeviceOffline,
        Errno::PermissionDenied,
        Errno::NotFound,
    ] {
        let mut host = service().with_sealed_refusal(err);
        assert_eq!(
            Vault::open(&mut host).err(),
            Some(err),
            "a vault that cannot be read must not open empty"
        );
    }
}

#[test]
fn a_failed_sealed_write_leaves_the_handle_as_it_was() {
    let mut host = service().with_sealed("token = abc\n");
    // Taken before the handle borrows the host: the switch is what lets a call
    // fail while a handle is live.
    let refuse = host.refusal();
    let mut vault = Vault::open(&mut host).expect("opens");
    refuse.set(Some(Errno::DeviceOffline));
    assert_eq!(
        vault.set("imap.password", "hunter2"),
        Err(Errno::DeviceOffline)
    );
    assert_eq!(
        vault.get("imap.password"),
        None,
        "a write that did not land must not appear to have"
    );
    assert_eq!(vault.get("token"), Some("abc"), "and nothing else changed");
}

#[test]
fn a_sealed_write_of_a_value_the_format_refuses_is_reported() {
    let mut host = service();
    let mut vault = Vault::open(&mut host).expect("opens");
    assert_eq!(vault.set("Not.A.Key", "v"), Err(Errno::OutOfRange));
    assert_eq!(vault.set("k", "a \u{7} bell"), Err(Errno::OutOfRange));
    drop(vault);
    assert_eq!(host.sealed().settings().count(), 0);
}

/// A sealed write ends by re-reading the store, so the handle reflects what the
/// service holds rather than what this library guessed — including a secret
/// another instance of the application sealed in the meantime.
#[test]
fn a_sealed_write_re_reads_what_the_service_then_holds() {
    let mut host = service();
    let mut vault = Vault::open(&mut host).expect("opens");
    vault.set("token", "first").expect("seals");
    assert_eq!(vault.get("token"), Some("first"));
    drop(vault);
    assert_eq!(host.calls(), 3, "the open, the write, and the re-read");
}

/// A write that lands and is then followed by a failed re-read reports the
/// re-read's error rather than claiming the write did not happen: it did, and a
/// caller that retries pays nothing because the value is already sealed.
#[test]
fn a_sealed_write_whose_re_read_fails_is_reported_but_still_landed() {
    let mut host = service();
    let refuse_reads = host.read_refusal();
    let mut vault = Vault::open(&mut host).expect("opens");
    refuse_reads.set(Some(Errno::DeviceOffline));
    assert_eq!(
        vault.set("token", "sealed"),
        Err(Errno::DeviceOffline),
        "the re-read failed, and that is what is reported"
    );
    assert_eq!(
        vault.get("token"),
        None,
        "the handle keeps its previous view rather than a guess"
    );
    // The write itself landed, so a retry once the volume is back costs
    // nothing and the handle catches up.
    refuse_reads.set(None);
    vault.set("token", "sealed").expect("already sealed");
    assert_eq!(vault.get("token"), Some("sealed"));
    drop(vault);
    assert_eq!(host.sealed().get("token"), Some("sealed"));
}

#[test]
fn a_sealed_read_negotiates_capacity_for_a_document_past_the_probe() {
    // A vault of many long secrets — a certificate store, say. The first call
    // asks for the probe size and is told the length; the second asks for
    // exactly that and gets the whole document. A prefix is never parsed.
    let mut text = String::new();
    let mut expected = 0usize;
    while text.len() <= super::READ_PROBE {
        let _ = core::fmt::Write::write_fmt(
            &mut text,
            format_args!("token.{expected} = {}\n", "s".repeat(120)),
        );
        expected += 1;
    }
    let mut host = service().with_sealed(&text);
    let vault = Vault::open(&mut host).expect("opens");
    assert!(vault.has("token.0"));
    assert!(
        vault.has(&alloc::format!("token.{}", expected - 1)),
        "the last secret of an oversize vault is present, so nothing was truncated"
    );
    drop(vault);
    assert_eq!(
        host.calls(),
        2,
        "one probe, one read at the declared length"
    );
}

/// The sealed and configuration scopes are separate documents to the client
/// too: neither handle can see the other's keys.
#[test]
fn the_sealed_scope_is_a_document_of_its_own() {
    let mut host = service()
        .with_store("font.size = 14\n")
        .with_sealed("imap.password = hunter2\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    assert_eq!(settings.get("imap.password"), None);
    assert_eq!(settings.get("font.size"), Some("14"));
    drop(settings);
    let vault = Vault::open(&mut host).expect("opens");
    assert_eq!(vault.get("font.size"), None);
    assert_eq!(vault.get("imap.password"), Some("hunter2"));
}
