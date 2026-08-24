//! `cargo xtask charter-cite` implementation.
//!
//! The charter forbids a comment from citing one of its own section numbers: a
//! section number restates *what* a rule is, where a comment must say *why*
//! the code does what it does. References to anything *outside* the charter —
//! a plan, a `docs/` page, an RFC, a hardware manual — are legitimate and must
//! survive, so the scan distinguishes the two by the source named next to the
//! reference rather than by the notation alone.
//!
//! Two rules, both over comments in tracked `.rs` sources:
//!
//! 1. A comment must not name the charter file beside a section reference.
//!    Naming it in prose ("the charter forbids this duplication") is fine; it
//!    is the section number that restates the rule instead of the reason.
//! 2. A `§N` whose number is one of the charter's own section labels must have
//!    a source named just before it, so `RFC 9293 §3.2` and
//!    `` `plans/APPS.md` §4 `` pass while a bare `(§2.2)` — which a reader can
//!    only resolve as the charter — does not. The source may sit anywhere in
//!    the same comment paragraph, so a reference wrapped across lines still
//!    reads as anchored.
//!
//! A generated file is skipped: the charter's one sanctioned citation is the
//! provenance a generator stamps onto what it emits, and hand-editing that
//! banner to satisfy this check would only make the generated view drift from
//! its generator. Fix such a banner in the generator, which is hand-written
//! code and is scanned.
//!
//! The label set is read from `AGENTS.md` itself, so the check tracks the
//! charter rather than a copy of its table of contents.

use std::collections::BTreeSet;
use std::path::Path;

/// The charter file, both the label source and the name rule 1 refuses.
const CHARTER: &str = "AGENTS.md";

/// This checker's own file, skipped because it necessarily names [`CHARTER`].
const SELF_FILE: &str = "charter_cite.rs";

/// The banner every in-tree generator stamps onto what it emits.
const GENERATED: &str = "GENERATED FILE";

/// Whether `src` declares itself generated.
///
/// Every in-tree generator writes the banner as the emitted file's *first*
/// line, as a plain `//` comment. Requiring exactly that keeps a hand-written
/// generator — whose own `//!` module doc naturally mentions the banner it
/// writes — inside the scan.
fn is_generated(src: &str) -> bool {
    let Some(first) = src.lines().next().map(str::trim_start) else {
        return false;
    };
    first.starts_with("//")
        && !first.starts_with("///")
        && !first.starts_with("//!")
        && first.contains(GENERATED)
}

/// How far back from a `§` a named source is looked for. One clause: long
/// enough for `` (`docs/src/filesystem/arxfs-spec.md` §11) `` to anchor its
/// reference, short enough that an unrelated earlier sentence cannot.
const LOOKBEHIND: usize = 45;

/// Source names that anchor a section reference to something outside the
/// charter — every one of them cited by section somewhere in the tree, so the
/// list is evidence rather than a wish list. A reference whose source is named
/// only further away than [`LOOKBEHIND`] must repeat the name; that is the
/// price of an unambiguous citation and is what the diagnostic asks for. A
/// newly-cited specification adds its name here, in the same change.
///
/// A lowercase word that is also a substring of ordinary prose (`spec` in
/// "specific") carries a trailing space, so it anchors only where it is
/// actually the source: `spec §13`.
const SOURCES: &[&str] = &[
    // In-tree documents: a plan, a `docs/` page, or the shorthand a crate's own
    // module doc anchors.
    ".md",
    "plan ",
    "spec ",
    "SYSLOG",
    // Requests for comments and other numbered standards.
    "RFC",
    "ISO",
    "ITU",
    "IANA",
    // Bus, controller, and device specifications.
    "virtio",
    "xHCI",
    "USB",
    "UAS",
    "CBI",
    "BOT",
    "UFI",
    "PCI",
    "ATA",
    "SPC-4",
    "SBC-3",
    "SAM-5",
    "ACPI",
    "UEFI",
    "Multiboot",
    "VBE",
    "GICv",
    "HID",
    "VL805",
    // Architectures and vendors whose manuals are cited by section.
    "Arm",
    "ARM",
    "RISC-V",
    "Intel",
    "AMD",
    "SDM",
    // Wire and file formats.
    "DHCP",
    "DNS",
    "ICMP",
    "TCP",
    "Ethernet",
    "PNG",
    "T.81",
    "ext4",
];

/// A comment that cites the charter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub line: usize,
    /// Why the citation was refused, phrased for the developer.
    pub reason: &'static str,
    pub text: String,
}

/// Read the charter's own section labels (`2.11`, `19.10`, …).
///
/// Three sources, all derived rather than copied, so the set tracks the charter
/// as it is edited: every `§N` the charter cites, its `##`/`###` headings, and
/// the ordered-list items under each heading — the charter numbers its rules as
/// list items (`§2.11`, `§5.4.5`), which no heading records.
pub fn charter_labels(root: &Path) -> Result<BTreeSet<String>, String> {
    let path = root.join(CHARTER);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("charter-cite: cannot read {}: {e}", path.display()))?;
    let mut out = BTreeSet::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find('§') {
        rest = &rest[at + '§'.len_utf8()..];
        if let Some(label) = leading_label(rest) {
            out.insert(label);
        }
    }
    let mut section: Option<String> = None;
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let head = line.trim_start_matches('#');
        if head.len() < line.len() {
            section = leading_label(head.trim_start());
            if let Some(label) = section.clone() {
                out.insert(label);
            }
            continue;
        }
        // `1. **No hacks.**` under `## 2.` is §2.1; the charter's rules are
        // numbered nowhere else.
        if let (Some(sec), Some(item)) = (section.as_deref(), ordered_item(line)) {
            out.insert(format!("{sec}.{item}"));
        }
    }
    Ok(out)
}

/// The number of an ordered-list item line (`  3. text` → `3`).
fn ordered_item(line: &str) -> Option<u32> {
    let (digits, _) = line.trim_start().split_once(". ")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The dotted decimal label at the start of `s`, if any (`2.11 foo` → `2.11`).
fn leading_label(s: &str) -> Option<String> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let label = s[..end].trim_end_matches('.');
    if label.is_empty() || !label.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(label.to_string())
}

/// Where a source scan is when it reaches a line boundary.
///
/// A section number inside a string literal is program output, which the
/// charter permits to name the rule a developer violated — a `compile_error!`
/// message, or the provenance banner a generator emits into a generated file.
/// Those literals span lines, so the scanner has to carry its state across
/// them rather than judging each line alone.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Lex {
    #[default]
    Code,
    /// Inside a `"…"` literal (possibly continued with a trailing `\\`).
    Str,
    /// Inside an `r#"…"#` literal, holding its hash count.
    Raw(usize),
    /// Inside a `/* … */` comment, holding its nesting depth.
    Block(usize),
}

/// Advance `state` across `line`, returning the line's `//` comment body.
fn comment_body<'a>(line: &'a str, state: &mut Lex) -> Option<&'a str> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut k = 0;
    let at = |k: usize| chars.get(k).map(|(_, c)| *c);
    let byte = |k: usize| chars.get(k).map_or(line.len(), |(i, _)| *i);
    while k < chars.len() {
        match *state {
            Lex::Str => {
                match at(k) {
                    Some('\\') => k += 1,
                    Some('"') => *state = Lex::Code,
                    _ => {}
                }
                k += 1;
            }
            Lex::Raw(h) => {
                if at(k) == Some('"') && (1..=h).all(|d| at(k + d) == Some('#')) {
                    *state = Lex::Code;
                    k += 1 + h;
                } else {
                    k += 1;
                }
            }
            Lex::Block(depth) => {
                if at(k) == Some('*') && at(k + 1) == Some('/') {
                    *state = if depth == 1 {
                        Lex::Code
                    } else {
                        Lex::Block(depth - 1)
                    };
                    k += 2;
                } else if at(k) == Some('/') && at(k + 1) == Some('*') {
                    *state = Lex::Block(depth + 1);
                    k += 2;
                } else {
                    k += 1;
                }
            }
            Lex::Code => match at(k) {
                Some('/') if at(k + 1) == Some('/') => return Some(&line[byte(k)..]),
                Some('/') if at(k + 1) == Some('*') => {
                    *state = Lex::Block(1);
                    k += 2;
                }
                Some('"') => {
                    *state = Lex::Str;
                    k += 1;
                }
                Some('r') => {
                    let h = (1..chars.len() - k)
                        .take_while(|d| at(k + d) == Some('#'))
                        .count();
                    if at(k + 1 + h) == Some('"') {
                        *state = Lex::Raw(h);
                        k += 2 + h;
                    } else {
                        k += 1;
                    }
                }
                // A char literal cannot open a string, so skip its body: `'"'`
                // must not read as a quote.
                Some('\'') => {
                    let len = if at(k + 1) == Some('\\') { 2 } else { 1 };
                    k += if at(k + 1 + len) == Some('\'') {
                        2 + len
                    } else {
                        1
                    };
                }
                _ => k += 1,
            },
        }
    }
    // A `"…\` continuation keeps the literal open across the newline; without
    // one the literal is closed by the line's end.
    if *state == Lex::Str && !line.trim_end().ends_with('\\') {
        *state = Lex::Code;
    }
    None
}

/// Scan one comment paragraph for citations, each with its byte offset in
/// `text` so the caller can name the line it sits on.
///
/// `text` is the paragraph's lines joined by a space, so a reference wrapped
/// across lines still finds the source named before it.
fn scan_paragraph(text: &str, labels: &BTreeSet<String>) -> Vec<(usize, &'static str)> {
    let mut out = Vec::new();
    if text.contains('§') {
        if let Some(at) = text.find(CHARTER) {
            out.push((at, "cites the charter by section"));
        }
    }
    let mut rest = text;
    let mut consumed = 0usize;
    while let Some(at) = rest.find('§') {
        let abs = consumed + at;
        rest = &rest[at + '§'.len_utf8()..];
        consumed = abs + '§'.len_utf8();
        // `§X`, `§SYSRET`, and `§"Overflow"` name a section that has no number;
        // only a sign attached to nothing is a leftover.
        let Some(label) = leading_label(rest) else {
            if rest.starts_with(char::is_whitespace) || rest.is_empty() {
                out.push((abs, "section sign naming no section"));
            }
            continue;
        };
        if !labels.contains(&label) {
            continue;
        }
        let start = text[..abs]
            .char_indices()
            .rev()
            .take(LOOKBEHIND)
            .last()
            .map_or(0, |(i, _)| i);
        if !SOURCES.iter().any(|src| text[start..abs].contains(src)) {
            out.push((abs, "section number with no source named beside it"));
        }
    }
    out
}

/// Scan the workspace rooted at `root` for comments that cite the charter.
pub fn scan(root: &Path) -> Result<Vec<Violation>, String> {
    let labels = charter_labels(root)?;
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("charter-cite: cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("charter-cite: dir entry: {e}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("charter-cite: file type {}: {e}", path.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                if name == "target" || name == ".git" {
                    continue;
                }
                dirs.push(path);
            } else if file_type.is_file() && name.ends_with(".rs") && name != SELF_FILE {
                scan_file(&path, &relative(root, &path), &labels, &mut out)?;
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn scan_file(
    path: &Path,
    rel: &str,
    labels: &BTreeSet<String>,
    out: &mut Vec<Violation>,
) -> Result<(), String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("charter-cite: cannot read {}: {e}", path.display()))?;
    if is_generated(&src) {
        return Ok(());
    }
    // A paragraph is a run of comment lines with text; the citations in one are
    // scanned together so a wrapped reference keeps the source named before it.
    let mut state = Lex::default();
    let mut paragraphs: Vec<Vec<(usize, &str)>> = Vec::new();
    let mut para: Vec<(usize, &str)> = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let body = comment_body(line, &mut state)
            .map(|b| b.trim_start_matches('/').trim_start_matches('!').trim())
            .filter(|b| !b.is_empty());
        match body {
            Some(b) => para.push((idx + 1, b)),
            None if !para.is_empty() => paragraphs.push(std::mem::take(&mut para)),
            None => {}
        }
    }
    if !para.is_empty() {
        paragraphs.push(para);
    }
    for para in &paragraphs {
        let text = para.iter().map(|(_, b)| *b).collect::<Vec<_>>().join(" ");
        for (at, reason) in scan_paragraph(&text, labels) {
            let Some((line, body)) = line_of(para, at) else {
                continue;
            };
            out.push(Violation {
                path: rel.to_string(),
                line,
                reason,
                text: body.to_string(),
            });
        }
    }
    Ok(())
}

/// The paragraph line carrying byte offset `at` of the joined text.
///
/// `None` only for an empty paragraph, which carries no citation to report.
fn line_of<'a>(para: &[(usize, &'a str)], at: usize) -> Option<(usize, &'a str)> {
    let mut base = 0usize;
    let mut last = None;
    for entry in para {
        last = Some(*entry);
        if at < base + entry.1.len() + 1 {
            return last;
        }
        base += entry.1.len() + 1;
    }
    last
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Run the check, printing a report and failing when any citation remains.
pub fn run(root: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let violations = scan(root)?;
    if violations.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "charter-cite: a comment must state the reason, not cite a charter \
         section (AGENTS.md §2.11 / §15.17). Replace each with the prose reason \
         (\"fail closed\", \"zeroed on drop\"), or, for a reference to another \
         document, name that document beside the section number:\n",
    );
    for v in &violations {
        let _ = writeln!(msg, "  {}:{}: {} — {}", v.path, v.line, v.reason, v.text);
    }
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    }

    fn labels() -> BTreeSet<String> {
        charter_labels(&workspace_root()).expect("charter labels")
    }

    #[test]
    fn workspace_carries_no_charter_citations() {
        let violations = scan(&workspace_root()).expect("scan");
        assert!(
            violations.is_empty(),
            "unexpected charter citations in comments: {violations:#?}"
        );
    }

    #[test]
    fn charter_labels_cover_the_rule_lists_and_the_headings() {
        let l = labels();
        for want in [
            "2.11", "2.24", "5.4", "15.17", "16.8", "19.10", "23.5", "26.7", "27.5",
        ] {
            assert!(l.contains(want), "missing charter label {want}");
        }
        assert!(
            !l.contains("6.2.2"),
            "an xHCI section is not a charter label"
        );
    }

    #[test]
    fn naming_the_charter_is_refused_in_every_comment_spelling() {
        for body in [
            "//! the one definition (`AGENTS.md` §2.2)",
            "/// fails closed (AGENTS.md §5.4)",
        ] {
            assert!(
                reasons(body).contains(&"cites the charter by section"),
                "not refused: {body}"
            );
        }
    }

    #[test]
    fn a_bare_charter_section_is_refused() {
        for body in [
            "/// one definition (§2.2).",
            "// fail closed (§5.4)",
            "//! never a linear scan under §26 load",
            "/// a fixed bound, not a capacity (§24.4)",
        ] {
            assert_eq!(
                reasons(body),
                vec!["section number with no source named beside it"],
                "not refused: {body}"
            );
        }
    }

    #[test]
    fn naming_the_charter_in_prose_is_accepted() {
        // The charter asks for the reason, and permits naming itself in prose.
        assert!(reasons("/// rather than retrying forever (the `AGENTS.md` ban).").is_empty());
    }

    #[test]
    fn a_sourced_reference_is_accepted() {
        for body in [
            "/// The RFC 9293 §3.3.2 connection states.",
            "//! (`plans/APPS.md` §4): one document per command.",
            "/// Endpoint context type field: Control (xHCI §6.2.3).",
            "/// Power-on-good ceiling (USB 2.0 §11.11).",
            "//! (`docs/src/filesystem/arxfs-spec.md` §11) queries the array.",
            "/// Split virtqueue management (virtio 1.1 §2.6).",
            "// the spec §13 authority treatment",
        ] {
            assert!(
                reasons(body).is_empty(),
                "wrongly refused: {body} -> {:?}",
                reasons(body)
            );
        }
    }

    /// Comment bodies of `src`, threading the lexer state across its lines.
    fn bodies(src: &str) -> Vec<&str> {
        let mut state = Lex::default();
        src.lines()
            .filter_map(|l| comment_body(l, &mut state))
            .collect()
    }

    /// The reasons [`scan_paragraph`] gives for one comment body.
    fn reasons(body: &str) -> Vec<&'static str> {
        let text = body.trim_start_matches('/').trim_start_matches('!').trim();
        scan_paragraph(text, &labels())
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    }

    #[test]
    fn a_section_number_in_a_string_literal_is_program_output() {
        // A build or CI diagnostic may name the rule a developer violated.
        let src = r#"    compile_error!("exactly one scheduler feature (AGENTS.md §17.1)");"#;
        assert!(bodies(src).is_empty());
    }

    #[test]
    fn a_generated_file_is_skipped() {
        // The charter sanctions a generator stamping the governing rule onto
        // what it emits; the fix for such a banner is in the generator.
        let dir = std::env::temp_dir().join(format!("tairix-cc-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("atlas.rs");
        std::fs::write(
            &file,
            "// GENERATED FILE — DO NOT EDIT.\n\
             //\n\
             // (AGENTS.md §2.2: generated views are never hand-maintained).\n",
        )
        .expect("write");
        let mut out = Vec::new();
        scan_file(&file, "atlas.rs", &labels(), &mut out).expect("scan");
        std::fs::remove_dir_all(&dir).ok();
        assert!(out.is_empty(), "{out:#?}");
    }

    #[test]
    fn a_hand_written_file_mentioning_generated_files_is_still_scanned() {
        let dir = std::env::temp_dir().join(format!("tairix-cc-hand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("gen.rs");
        std::fs::write(
            &file,
            "//! Writes a GENERATED FILE banner.\n\
             //!\n\
             //! One definition (§2.2).\n",
        )
        .expect("write");
        let mut out = Vec::new();
        scan_file(&file, "gen.rs", &labels(), &mut out).expect("scan");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(
            out[0].reason,
            "section number with no source named beside it"
        );
    }

    #[test]
    fn a_generator_provenance_banner_is_program_output() {
        // The charter's one sanctioned citation: a generator stamping which
        // rule governs the artefact it emits. The literal spans lines, so the
        // scanner must not read its continuations as comments.
        let src = "        out.push_str(\n\
                   \x20            \"// GENERATED FILE — DO NOT EDIT.\\n\\\n\
                   \x20            // (AGENTS.md §2.2: generated views are never hand-maintained).\\n\",\n\
                   \x20        );";
        assert!(bodies(src).is_empty(), "{:?}", bodies(src));
    }

    #[test]
    fn a_comment_after_a_string_is_still_scanned() {
        let src = r#"    let s = "a // b"; // one definition (§2.2)"#;
        assert_eq!(bodies(src), vec!["// one definition (§2.2)"]);
        assert_eq!(
            reasons(bodies(src)[0]),
            vec!["section number with no source named beside it"]
        );
    }

    #[test]
    fn a_raw_string_and_a_char_literal_do_not_open_a_literal() {
        let src = "    let q = \'\"\'; let r = r#\"a \"b\" c\"#; // fail closed (§5.4)";
        assert_eq!(bodies(src), vec!["// fail closed (§5.4)"]);
    }

    #[test]
    fn a_blank_doc_line_separates_paragraphs() {
        // `//!` must read as blank, or a whole module doc becomes one
        // paragraph and a citation borrows an unrelated source.
        let src = "//! (`plans/APPS.md` §4) names the source.\n//!\n//! one definition (§2.2).";
        let mut state = Lex::default();
        let bodies: Vec<&str> = src
            .lines()
            .filter_map(|l| comment_body(l, &mut state))
            .map(|b| b.trim_start_matches('/').trim_start_matches('!').trim())
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert!(scan_paragraph(bodies[0], &labels()).is_empty());
        assert_eq!(
            scan_paragraph(bodies[1], &labels())
                .into_iter()
                .map(|(_, r)| r)
                .collect::<Vec<_>>(),
            vec!["section number with no source named beside it"]
        );
    }

    #[test]
    fn a_section_sign_attached_to_nothing_is_refused() {
        assert_eq!(
            reasons("// authentication must fail (§ relocation defence)"),
            vec!["section sign naming no section"]
        );
    }

    #[test]
    fn a_section_named_rather_than_numbered_is_accepted() {
        for body in [
            "/// (`plans/PI.md` §X), the riscv64 sibling of the aarch64 one.",
            "//! `STAR[63:48] + 8` (SDM Vol 2B §SYSRET).",
            "/// (`docs/src/architecture/scheduler.md` §\"Starvation freedom\").",
        ] {
            assert!(reasons(body).is_empty(), "wrongly refused: {body}");
        }
    }

    #[test]
    fn a_source_named_earlier_in_the_paragraph_anchors_a_wrapped_reference() {
        let text = "The scrub report (`docs/src/filesystem/arxfs-spec.md` \
                    §12). Counts accumulate across the resumable calls.";
        assert!(scan_paragraph(text, &labels()).is_empty());
    }
}
