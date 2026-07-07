//! The one help-tree lint (`plans/APPS.md` §8.1) — completeness, structure,
//! switch-key drift, and content policy over a set of discovered `Help/`
//! trees.
//!
//! `cargo xtask help-lint` and the `tools/syshelp` aggregator tests both
//! judge the same discovered documents, so the judgement lives here, in the
//! engine that owns the document model — never re-derived per consumer.
//! The lint is pure: it receives the discovered rows as data and returns the
//! violations as messages; it performs no I/O and grants nothing.
//!
//! What it checks, per `plans/APPS.md`:
//!
//! * **Spellings** — every locale directory parses as a [`Locale`] and every
//!   file name is a `.md` document whose stem parses as a [`DocumentName`]
//!   (§2.1).
//! * **Bounds** — every document parses whole under [`HelpDoc::parse`]'s
//!   fail-closed limits (§6), so a malformed page never reaches an image.
//! * **Completeness** — a bundle that ships help ships a canonical `en-US/`
//!   document, every document exists in each [`REQUIRED_LOCALES`] directory,
//!   and no translation carries a document absent from `en-US/` (§2.1,
//!   §8.1).
//! * **Switch keys** — every `OPTIONS` list item leads with a backticked,
//!   language-neutral switch key, and each translation's key sequence equals
//!   `en-US/`'s exactly (§3.1): the flags are properties of the parser,
//!   never of the language.
//! * **Content policy** — no document, in any locale, contains a word from
//!   the closed [`DISALLOWED_WORDS`] screen (§8.1).
//!
//! The per-app unit tests still pin `en-US/`'s `OPTIONS` to each program's
//! *actual* argument parser (§3.1) — only the app crate knows its parser;
//! this lint pins every translation to that already-pinned canonical set.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::doc::{Block, HelpDoc, SectionKind, Span};
use crate::locale::{DocumentName, Locale, DEFAULT_LOCALE, REQUIRED_LOCALES};

/// One discovered help document for the lint to judge: its bundle directory
/// name (`ls.app`), locale directory (`en-US`, `fr-FR`, …), file name
/// (`ls.md`), and raw bytes.
#[derive(Clone, Copy, Debug)]
pub struct LintDoc<'a> {
    /// The bundle directory name, including the `.app` suffix.
    pub bundle: &'a str,
    /// The locale directory name.
    pub locale: &'a str,
    /// The document file name.
    pub file: &'a str,
    /// The document's bytes.
    pub bytes: &'a [u8],
}

/// The closed content-policy screen (`plans/APPS.md` §8.1): profane or
/// derogatory words, lower-case, matched on whole alphabetic words in any
/// locale's document. A heuristic word list is a screen, not a proof — a
/// reviewer still reads the prose — but it fails the obvious cases closed.
const DISALLOWED_WORDS: &[&str] = &[
    // en
    "fuck",
    "shit",
    "cunt",
    "wanker",
    "asshole",
    "arsehole",
    "bitch",
    "bastard",
    // fr-FR
    "merde",
    "putain",
    "connard",
    "connasse",
    "salope",
    "enculé",
    // de-DE
    "scheisse",
    "scheiße",
    "arschloch",
    "fotze",
    "hurensohn",
    // es-ES
    "mierda",
    "joder",
    "gilipollas",
    "cabrón",
    "puta",
    // it-IT
    "cazzo",
    "merda",
    "stronzo",
    "vaffanculo",
    "puttana",
    // uk-UA
    "блядь",
    "хуй",
    "сука",
    "пізда",
    "підор",
    "курва",
    // pt-PT
    "caralho",
    "foda",
    "foder",
    "cabrão",
    // cy-GB
    "cachu",
    "ffwcio",
    "cont",
    // ko-KR
    "씨발",
    "개새끼",
    "병신",
    "지랄",
    // ar-SA
    "كس",
    "طيز",
    "زبي",
    "شرموطة",
    // he-IL
    "זיון",
    "כוסית",
    "זונה",
];

/// The closed content-policy screen for languages written without word
/// separators (`plans/APPS.md` §8.1): Chinese and Japanese vulgarities are
/// matched as substrings of a document's text, because whole-word matching
/// cannot segment continuous CJK prose. Each entry is long and specific
/// enough that an innocent embedding is implausible.
const DISALLOWED_CJK_SUBSTRINGS: &[&str] = &[
    // zh-CN
    "他妈的",
    "操你妈",
    "傻逼",
    "混蛋",
    // ja-JP
    "くたばれ",
    "クソ野郎",
    "ちくしょう",
];

/// Lint a set of discovered help documents.
///
/// Returns every violation as a human-readable message naming the offending
/// `bundle/locale/file`; an empty result is a passing tree. The order is
/// deterministic: per-document findings in row order, then per-bundle
/// structural findings in bundle order.
#[must_use]
pub fn lint_help_trees(docs: &[LintDoc<'_>]) -> Vec<String> {
    let mut violations = Vec::new();
    // The parsed OPTIONS switch-key sequence per document: `None` means the
    // document has no OPTIONS section; a document that failed any per-row
    // check is absent and excluded from the cross-locale comparison (its own
    // violation already fails the lint).
    let mut option_keys: BTreeMap<(&str, &str, &str), Option<Vec<String>>> = BTreeMap::new();

    for doc in docs {
        let at = format!("{}/{}/{}", doc.bundle, doc.locale, doc.file);
        let mut row_ok = true;
        if Locale::parse(doc.locale).is_err() {
            violations.push(format!("{at}: `{}` is not a valid locale", doc.locale));
            row_ok = false;
        }
        if let Some(stem) = doc.file.strip_suffix(".md") {
            if DocumentName::parse(stem).is_err() {
                violations.push(format!("{at}: `{stem}` is not a valid document name"));
                row_ok = false;
            }
        } else {
            violations.push(format!("{at}: not a `.md` document"));
            row_ok = false;
        }
        match HelpDoc::parse(doc.bytes) {
            Ok(parsed) => {
                match options_switch_keys(&at, &parsed) {
                    Ok(keys) => {
                        if row_ok {
                            option_keys.insert((doc.bundle, doc.locale, doc.file), keys);
                        }
                    }
                    Err(mut item_violations) => violations.append(&mut item_violations),
                }
                if let Ok(text) = core::str::from_utf8(doc.bytes) {
                    violations.extend(content_policy_violations(&at, text));
                }
            }
            Err(err) => violations.push(format!("{at}: does not parse: {err}")),
        }
    }

    violations.extend(tree_violations(docs, &option_keys));
    violations
}

/// The per-bundle structural findings: `en-US/` presence, required-locale
/// completeness, no translation-only documents, and cross-locale `OPTIONS`
/// switch-key drift.
fn tree_violations(
    docs: &[LintDoc<'_>],
    option_keys: &BTreeMap<(&str, &str, &str), Option<Vec<String>>>,
) -> Vec<String> {
    let mut violations = Vec::new();
    let mut bundles: BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> = BTreeMap::new();
    for doc in docs {
        bundles
            .entry(doc.bundle)
            .or_default()
            .entry(doc.locale)
            .or_default()
            .insert(doc.file);
    }
    for (bundle, locales) in &bundles {
        let Some(default_files) = locales.get(DEFAULT_LOCALE) else {
            violations.push(format!("{bundle}: no canonical {DEFAULT_LOCALE}/ document"));
            continue;
        };
        for (locale, files) in locales {
            for file in files {
                if !default_files.contains(file) {
                    violations.push(format!(
                        "{bundle}/{locale}/{file}: no {DEFAULT_LOCALE}/ counterpart"
                    ));
                }
            }
        }
        for file in default_files {
            for required in REQUIRED_LOCALES {
                if !locales
                    .get(required)
                    .is_some_and(|files| files.contains(file))
                {
                    violations.push(format!("{bundle}: {file} is missing locale {required}/"));
                }
            }
            let Some(default_keys) = option_keys.get(&(*bundle, DEFAULT_LOCALE, *file)) else {
                continue;
            };
            for (locale, files) in locales {
                if *locale == DEFAULT_LOCALE || !files.contains(file) {
                    continue;
                }
                let Some(keys) = option_keys.get(&(*bundle, *locale, *file)) else {
                    continue;
                };
                if keys != default_keys {
                    violations.push(format!(
                        "{bundle}/{locale}/{file}: OPTIONS switch keys {} differ from \
                         {DEFAULT_LOCALE}/'s {} — the flags are language-neutral",
                        describe_keys(keys.as_deref()),
                        describe_keys(default_keys.as_deref()),
                    ));
                }
            }
        }
    }
    violations
}

/// Extract the language-neutral switch keys from a parsed document's
/// `OPTIONS` section, in document order: the leading backticked code span of
/// each list item (`plans/APPS.md` §3.1). `Ok(None)` means the document has
/// no `OPTIONS` section; an item that does not lead with a backticked key is
/// a violation.
fn options_switch_keys(at: &str, doc: &HelpDoc) -> Result<Option<Vec<String>>, Vec<String>> {
    let Some(section) = doc.section(SectionKind::Options) else {
        return Ok(None);
    };
    let mut keys = Vec::new();
    let mut violations = Vec::new();
    for block in &section.blocks {
        let Block::List { items, .. } = block else {
            continue;
        };
        for item in items {
            match item.spans.first() {
                Some(Span::Code(key)) => keys.push(key.clone()),
                _ => violations.push(format!(
                    "{at}: OPTIONS item does not lead with a backticked switch key"
                )),
            }
        }
    }
    if violations.is_empty() {
        Ok(Some(keys))
    } else {
        Err(violations)
    }
}

/// Render a switch-key sequence (or the absence of an `OPTIONS` section)
/// for a drift message.
fn describe_keys(keys: Option<&[String]>) -> String {
    match keys {
        None => String::from("(no OPTIONS section)"),
        Some(keys) => {
            let mut out = String::from("[");
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push('`');
                out.push_str(key);
                out.push('`');
            }
            out.push(']');
            out
        }
    }
}

/// The content-policy findings for one document's text: every whole
/// alphabetic word, lower-cased, is matched against [`DISALLOWED_WORDS`],
/// and the whole text is screened for the [`DISALLOWED_CJK_SUBSTRINGS`]
/// entries, which no word split can find in continuous CJK prose.
/// Whole-word matching keeps an innocent containing word (e.g. a name that
/// merely embeds a banned spelling) clean.
fn content_policy_violations(at: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for word in text.split(|c: char| !c.is_alphabetic()) {
        if word.is_empty() {
            continue;
        }
        let lowered = word.to_lowercase();
        if DISALLOWED_WORDS.contains(&lowered.as_str()) {
            violations.push(format!(
                "{at}: disallowed word `{lowered}` (content policy, plans/APPS.md §8.1)"
            ));
        }
    }
    for banned in DISALLOWED_CJK_SUBSTRINGS {
        if text.contains(banned) {
            violations.push(format!(
                "{at}: disallowed phrase `{banned}` (content policy, plans/APPS.md §8.1)"
            ));
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write as _;

    use super::{lint_help_trees, LintDoc, REQUIRED_LOCALES};

    /// A minimal valid document whose `OPTIONS` documents `keys` (rendered
    /// one list item per key).
    fn doc_with_keys(keys: &[&str]) -> String {
        let mut text = String::from(
            "## NAME\n\nx — a tool\n\n## SYNOPSIS\n\n`x`\n\n## DESCRIPTION\n\nDoes x.\n",
        );
        if !keys.is_empty() {
            text.push_str("\n## OPTIONS\n\n");
            for key in keys {
                // Writing into a `String` is infallible.
                let _ = writeln!(text, "- `{key}` — a switch");
            }
        }
        text
    }

    /// A complete, clean tree: one document in every required locale.
    fn clean_tree(text: &str) -> Vec<(&'static str, String)> {
        REQUIRED_LOCALES
            .iter()
            .map(|locale| (*locale, String::from(text)))
            .collect()
    }

    fn rows<'a>(tree: &'a [(&'static str, String)]) -> Vec<LintDoc<'a>> {
        tree.iter()
            .map(|(locale, text)| LintDoc {
                bundle: "x.app",
                locale,
                file: "x.md",
                bytes: text.as_bytes(),
            })
            .collect()
    }

    #[test]
    fn complete_clean_tree_passes() {
        let tree = clean_tree(&doc_with_keys(&["-a, --all", "-h, -?"]));
        assert_eq!(lint_help_trees(&rows(&tree)), Vec::<String>::new());
    }

    #[test]
    fn tree_without_options_sections_passes() {
        let tree = clean_tree(&doc_with_keys(&[]));
        assert_eq!(lint_help_trees(&rows(&tree)), Vec::<String>::new());
    }

    #[test]
    fn missing_required_locale_is_flagged() {
        let mut tree = clean_tree(&doc_with_keys(&[]));
        tree.retain(|(locale, _)| *locale != "uk-UA");
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("missing locale uk-UA/"));
    }

    #[test]
    fn missing_default_is_flagged() {
        let mut tree = clean_tree(&doc_with_keys(&[]));
        tree.retain(|(locale, _)| *locale != "en-US");
        let violations = lint_help_trees(&rows(&tree));
        // One "no en-US/" finding per bundle; the per-file findings are
        // suppressed because there is no canonical set to compare against.
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("no canonical en-US/"));
    }

    #[test]
    fn translation_only_document_is_flagged() {
        let tree = clean_tree(&doc_with_keys(&[]));
        let extra = doc_with_keys(&[]);
        let mut docs = rows(&tree);
        docs.push(LintDoc {
            bundle: "x.app",
            locale: "fr-FR",
            file: "extra.md",
            bytes: extra.as_bytes(),
        });
        let violations = lint_help_trees(&docs);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("no en-US/ counterpart"));
    }

    #[test]
    fn malformed_document_is_flagged() {
        let mut tree = clean_tree(&doc_with_keys(&[]));
        tree[1].1 = String::from("no leading section heading\n");
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("does not parse"));
    }

    #[test]
    fn switch_key_drift_is_flagged() {
        let mut tree = clean_tree(&doc_with_keys(&["-a, --all", "-h, -?"]));
        tree[2].1 = doc_with_keys(&["-a, --alle", "-h, -?"]);
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("OPTIONS switch keys"),
            "{violations:?}"
        );
        assert!(violations[0].contains(tree[2].0), "{violations:?}");
    }

    #[test]
    fn missing_options_section_in_a_translation_is_drift() {
        let mut tree = clean_tree(&doc_with_keys(&["-a"]));
        tree[3].1 = doc_with_keys(&[]);
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("(no OPTIONS section)"),
            "{violations:?}"
        );
    }

    #[test]
    fn options_item_without_a_leading_key_is_flagged() {
        let mut text = doc_with_keys(&[]);
        text.push_str("\n## OPTIONS\n\n- plain prose, no backticked key\n");
        let tree = clean_tree(&text);
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), REQUIRED_LOCALES.len(), "{violations:?}");
        assert!(violations[0].contains("does not lead with a backticked switch key"));
    }

    #[test]
    fn disallowed_word_is_flagged_in_any_locale() {
        let mut tree = clean_tree(&doc_with_keys(&[]));
        tree[4].1 = tree[4].1.replace("Does x.", "Does merde.");
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("disallowed word"), "{violations:?}");
        assert!(violations[0].contains(tree[4].0), "{violations:?}");
    }

    #[test]
    fn disallowed_cjk_phrase_is_flagged_inside_continuous_prose() {
        // No word boundary separates the phrase from the surrounding CJK
        // prose, so only the substring screen can find it.
        let mut tree = clean_tree(&doc_with_keys(&[]));
        tree[1].1 = tree[1].1.replace("Does x.", "このツールはクソ野郎だ。");
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("disallowed phrase"),
            "{violations:?}"
        );
    }

    #[test]
    fn disallowed_word_matches_case_insensitively() {
        let mut tree = clean_tree(&doc_with_keys(&[]));
        tree[0].1 = tree[0].1.replace("Does x.", "Utter SHIT.");
        let violations = lint_help_trees(&rows(&tree));
        assert_eq!(violations.len(), 1, "{violations:?}");
    }

    #[test]
    fn innocent_containing_word_is_not_flagged() {
        // Whole-word matching: an embedding word is clean.
        let tree = clean_tree(&doc_with_keys(&[]).replace("Does x.", "The Scunthorpe cache."));
        assert_eq!(lint_help_trees(&rows(&tree)), Vec::<String>::new());
    }

    #[test]
    fn invalid_locale_and_file_spellings_are_flagged() {
        let text = doc_with_keys(&[]);
        let tree = clean_tree(&text);
        let mut docs = rows(&tree);
        docs.push(LintDoc {
            bundle: "x.app",
            locale: "not a locale",
            file: "x.md",
            bytes: text.as_bytes(),
        });
        docs.push(LintDoc {
            bundle: "x.app",
            locale: "fr-FR",
            file: "x.txt",
            bytes: text.as_bytes(),
        });
        let violations = lint_help_trees(&docs);
        assert!(
            violations.iter().any(|v| v.contains("not a valid locale")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("not a `.md` document")),
            "{violations:?}"
        );
    }
}
