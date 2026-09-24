//! Unit tests for the notification policy.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::notify_ipc::NotifySeverity;
use tairix_abi::{BundleId, BUNDLE_ID_MAX};
use tairix_appconf::MAX_VALUE_LEN;

use super::{NotifyLevel, NotifyPolicy, PolicyFull};

const SEVERITIES: [NotifySeverity; 4] = [
    NotifySeverity::Info,
    NotifySeverity::Success,
    NotifySeverity::Warning,
    NotifySeverity::Critical,
];

fn id(text: &str) -> BundleId {
    BundleId::new(text).expect("a bounded identity")
}

/// A legal bundle identity exactly `len` bytes long, distinct per `n`.
fn long_id(n: usize, len: usize) -> BundleId {
    let head = format!("a{n}.");
    let mut text = head.clone();
    text.extend(core::iter::repeat_n('b', len - head.len()));
    id(&text)
}

#[test]
fn a_fresh_policy_shows_everything_from_everyone() {
    let policy = NotifyPolicy::default();
    assert!(policy.enabled());
    for severity in SEVERITIES {
        assert!(policy.admits("os.tairix.netstack", severity));
    }
    assert_eq!(policy.sources().count(), 0);
    assert_eq!(policy.render_sources(), "");
}

#[test]
fn each_level_admits_exactly_its_severities() {
    let admitted = |level: NotifyLevel| -> Vec<NotifySeverity> {
        SEVERITIES
            .into_iter()
            .filter(|severity| level.admits(*severity))
            .collect()
    };
    assert_eq!(admitted(NotifyLevel::All), SEVERITIES.to_vec());
    assert_eq!(
        admitted(NotifyLevel::Warnings),
        [NotifySeverity::Warning, NotifySeverity::Critical].to_vec()
    );
    assert_eq!(
        admitted(NotifyLevel::Critical),
        [NotifySeverity::Critical].to_vec()
    );
    assert!(admitted(NotifyLevel::None).is_empty());
}

#[test]
fn the_desktop_switch_overrides_every_source() {
    let mut policy = NotifyPolicy::default();
    policy.set_enabled(false);
    for severity in SEVERITIES {
        assert!(!policy.admits("os.tairix.netstack", severity));
    }
}

#[test]
fn a_source_is_held_at_its_own_level_and_no_other() {
    let mut policy = NotifyPolicy::default();
    assert_eq!(
        policy.set_level(id("com.example.chat"), NotifyLevel::Critical),
        Ok(())
    );
    assert!(!policy.admits("com.example.chat", NotifySeverity::Warning));
    assert!(policy.admits("com.example.chat", NotifySeverity::Critical));
    assert!(policy.admits("com.example.mail", NotifySeverity::Info));
}

#[test]
fn setting_a_source_back_to_all_forgets_it() {
    let mut policy = NotifyPolicy::default();
    assert!(policy
        .set_level(id("com.example.chat"), NotifyLevel::None)
        .is_ok());
    assert!(policy
        .set_level(id("com.example.chat"), NotifyLevel::All)
        .is_ok());
    assert_eq!(policy, NotifyPolicy::default());
}

#[test]
fn the_spelling_is_sorted_and_round_trips() {
    let mut policy = NotifyPolicy::default();
    for (source, level) in [
        ("os.tairix.netstack", NotifyLevel::Warnings),
        ("com.example.chat", NotifyLevel::None),
        ("com.example.mail", NotifyLevel::Critical),
    ] {
        assert!(policy.set_level(id(source), level).is_ok());
    }
    let spelled = policy.render_sources();
    assert_eq!(
        spelled,
        "com.example.chat:none com.example.mail:critical os.tairix.netstack:warning"
    );
    let mut read = NotifyPolicy::default();
    assert!(read.set_sources(&spelled));
    assert_eq!(read, policy);
}

#[test]
fn any_order_and_spacing_reads_as_the_same_policy() {
    let mut policy = NotifyPolicy::default();
    assert!(policy.set_sources("  os.tairix.netstack:none \t com.example.chat:warning "));
    assert_eq!(
        policy.render_sources(),
        "com.example.chat:warning os.tairix.netstack:none"
    );
}

#[test]
fn a_malformed_entry_refuses_the_whole_value() {
    for value in [
        "com.example.chat",
        "com.example.chat:",
        ":none",
        "com.example.chat:loud",
        "Com.Example.Chat:none",
        "com..example:none",
        "com.example.chat:all",
        "com.example.chat:none com.example.chat:critical",
        "com.example.chat:none:none",
    ] {
        let mut policy = NotifyPolicy::default();
        assert!(policy.set_sources("com.example.mail:none"));
        let before = policy.clone();
        assert!(!policy.set_sources(value), "{value:?} must be refused");
        assert_eq!(policy, before, "a refusal must change nothing");
    }
}

#[test]
fn an_empty_value_is_the_policy_with_no_entries() {
    let mut policy = NotifyPolicy::default();
    assert!(policy.set_sources("com.example.chat:none"));
    assert!(policy.set_sources(""));
    assert_eq!(policy.sources().count(), 0);
}

#[test]
fn a_policy_that_would_outgrow_one_value_is_refused() {
    let mut policy = NotifyPolicy::default();
    let mut added = 0usize;
    while policy
        .set_level(long_id(added, BUNDLE_ID_MAX), NotifyLevel::Critical)
        .is_ok()
    {
        added += 1;
        assert!(added < 1000, "the policy never filled");
    }
    assert_eq!(
        policy.set_level(long_id(added, BUNDLE_ID_MAX), NotifyLevel::Critical),
        Err(PolicyFull)
    );
    let spelled = policy.render_sources();
    assert!(spelled.len() <= MAX_VALUE_LEN);
    // The worst case still leaves room for a policy worth having.
    assert!(added >= 13, "only {added} sources fit");
    // A level change on a held entry that would overflow is refused too.
    let mut tight = NotifyPolicy::default();
    let mut text = String::new();
    let mut n = 0;
    while text.len() + 1 + BUNDLE_ID_MAX + ":none".len() <= MAX_VALUE_LEN {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(long_id(n, BUNDLE_ID_MAX).as_str());
        text.push_str(":none");
        n += 1;
    }
    assert!(tight.set_sources(&text));
    let first = long_id(0, BUNDLE_ID_MAX);
    let slack = MAX_VALUE_LEN - tight.render_sources().len();
    let widened = tight.set_level(first, NotifyLevel::Critical);
    if slack < "critical".len() - "none".len() {
        assert_eq!(widened, Err(PolicyFull));
    } else {
        assert_eq!(widened, Ok(()));
    }
    assert!(tight.render_sources().len() <= MAX_VALUE_LEN);
}

#[test]
fn every_level_spelling_round_trips_and_is_distinct() {
    for level in NotifyLevel::ALL {
        assert_eq!(NotifyLevel::from_value(level.as_str()), Some(level));
    }
    assert_eq!(NotifyLevel::from_value("warnings"), None);
    assert_eq!(NotifyLevel::from_value(""), None);
}
