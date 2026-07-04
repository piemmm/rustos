//! `cargo xtask help-lint` implementation (`plans/APPS.md` §8.1).
//!
//! The gate that keeps every OS command app's `Help/` tree complete, well
//! formed, translation-consistent, and content-clean before it can reach an
//! image:
//!
//! 1. **The shared lint** (`rustos_help::lint_help_trees`, the one judgement
//!    the `tools/syshelp` aggregator tests also run) over every discovered
//!    help document (`rustos_syshelp::HELP_FILES` — the same build-discovered
//!    rows the image planters plant, so the linted set and the shipped set
//!    cannot drift): locale/document spellings, the fail-closed structural
//!    bounds, `default/` presence, required-locale completeness, no
//!    translation-only documents, cross-locale `OPTIONS` switch-key drift,
//!    and the content-policy screen.
//! 2. **Coverage**: every *command* app the `AppInfo.toml` discovery walk
//!    finds (`discover_app_manifests` — never a per-bundle list) must ship a
//!    `default/<command>.md` document for its own command word. Services are
//!    not commands and ship help only if they expose one.
//!
//! The per-app unit tests still pin `default/`'s `OPTIONS` to each program's
//! actual argument parser (`plans/APPS.md` §3.1) — only the app crate knows
//! its parser; this gate pins everything the parser cannot see.
//!
//! Any violation fails closed with a message naming the offending
//! `bundle/locale/file`; it is a defect fixed in the same change, never
//! waved through.

use std::collections::BTreeSet;

use rustos_help::{lint_help_trees, LintDoc, DEFAULT_LOCALE};
use rustos_itest_harness::app_image::{discover_app_manifests, AppKind};
use rustos_syshelp::HELP_FILES;

use crate::Context;

pub fn run(ctx: &Context) -> Result<(), String> {
    let docs: Vec<LintDoc<'_>> = HELP_FILES
        .iter()
        .map(|row| LintDoc {
            bundle: row.bundle,
            locale: row.locale,
            file: row.file,
            bytes: row.bytes,
        })
        .collect();
    let mut violations = lint_help_trees(&docs);

    // Coverage: every discovered command app ships its own command word's
    // canonical document. The discovery walk is the store's build-time
    // source of truth; a command app absent from the discovered help rows
    // shipped no `Help/` tree at all.
    let userland = ctx.workspace_root.join("userland");
    let discovered = discover_app_manifests(&userland).map_err(|e| format!("help-lint: {e}"))?;
    let default_docs: BTreeSet<(&str, &str)> = HELP_FILES
        .iter()
        .filter(|row| row.locale == DEFAULT_LOCALE)
        .map(|row| (row.bundle, row.file))
        .collect();
    for app in &discovered {
        if app.manifest.kind != AppKind::Command {
            continue;
        }
        let bundle = app.manifest.bundle_dir();
        let file = format!("{}.md", app.manifest.name);
        if !default_docs.contains(&(bundle.as_str(), file.as_str())) {
            violations.push(format!(
                "{bundle}: command app `{}` ships no {DEFAULT_LOCALE}/{file} help document",
                app.manifest.name
            ));
        }
    }

    if violations.is_empty() {
        let bundles: BTreeSet<&str> = HELP_FILES.iter().map(|row| row.bundle).collect();
        eprintln!(
            "xtask: help-lint: {} documents across {} bundles are clean",
            HELP_FILES.len(),
            bundles.len()
        );
        return Ok(());
    }
    for violation in &violations {
        eprintln!("xtask: help-lint: {violation}");
    }
    Err(format!("help-lint found {} violation(s)", violations.len()))
}
