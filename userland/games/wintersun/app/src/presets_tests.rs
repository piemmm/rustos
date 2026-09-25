//! The default preset ships, decodes, and is looked for where the bundle is
//! planted.

extern crate std;

use tairix_wintersun_figure::identity::{Identity, RECORD_EXTENSION};

use super::{installed, BUNDLE, DEFAULT};

#[test]
fn the_default_preset_ships_as_a_record() {
    let path = std::format!(
        "{}/Resources/{DEFAULT}.{RECORD_EXTENSION}",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).expect("the default preset ships");
    assert!(
        Identity::decode(&bytes).is_ok(),
        "the default preset is not a record"
    );
}

#[test]
fn the_bundle_is_the_application_the_manifest_names() {
    let manifest = include_str!("../AppInfo.toml");
    let line = |key: &str| {
        manifest
            .lines()
            .find_map(|line| line.strip_prefix(key)?.trim().strip_prefix('='))
            .map(|value| value.trim().trim_matches('"'))
    };
    assert_eq!(line("name"), Some(BUNDLE));
    assert_eq!(line("kind"), Some("application"));
    assert_eq!(
        installed(DEFAULT),
        "/System/Applications/wintersun.app/Resources/human-wayfarer.figure"
    );
}
