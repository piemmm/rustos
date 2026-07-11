//! Fixture-driven tests for the resolve/render engine: an in-memory bundle
//! store and console stand in for the `fs_*` and standard-stream syscalls,
//! so every behaviour — resolution order, locale fallback, the advisory
//! record, and the pager — is asserted without a kernel.

use core::cell::RefCell;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_log::{Event, Sink};
use rustos_sandbox::helpdoc::{HelpRenderFailure, HelpService};
use rustos_sandbox::host::ParserSandbox;
use rustos_sandbox::loopback::LoopbackLauncher;
use rustos_sandbox::worker::Service;

use crate::client::{Request, USAGE};
use crate::command::Command;
use crate::error::ManError;
use crate::io::{BundleStore, Console};

/// Discards every logged event (the loopback happy paths log nothing).
struct NullSink;

impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Drive [`crate::client::run`] over a fresh in-process loopback sandbox
/// running the real [`HelpService`] — the host-test stand-in for the `Run`
/// binary's re-spawned worker process.
fn run(
    command: &Command,
    request: &Request<'_>,
    store: &dyn BundleStore,
    console: &dyn Console,
) -> Result<(), ManError> {
    let mut sandbox = ParserSandbox::new(
        LoopbackLauncher::new(HelpService::default as fn() -> HelpService),
        NullSink,
    );
    crate::client::run(command, request, store, console, &mut sandbox)
}

/// A minimal well-formed Help document for `ps`.
const PS_DOC: &str = "## NAME\n\nps — list processes\n\n## SYNOPSIS\n\n`ps [-e]`\n\n## DESCRIPTION\n\nLists the caller's processes.\n";

/// The same document translated (the prose differs; the structure is the
/// same, as the content policy requires).
const PS_DOC_FR: &str = "## NAME\n\nps — lister les processus\n\n## SYNOPSIS\n\n`ps [-e]`\n\n## DESCRIPTION\n\nListe les processus de l'appelant.\n";

/// A second topic shipped by the `top` bundle.
const TOP_KEYS_DOC: &str = "## NAME\n\nkeys — top's interactive keys\n\n## SYNOPSIS\n\n`q`\n\n## DESCRIPTION\n\nThe keys the viewer accepts.\n";

/// `man`'s own document, served by its short-help switches.
const MAN_DOC: &str = "## NAME\n\nman — show a command's help\n\n## SYNOPSIS\n\n`man <command> [topic]`\n\n## DESCRIPTION\n\nRenders the Help document.\n";

/// One installed bundle: its directory and its `Help/` documents as
/// `(locale_dir, file_name, bytes)` rows.
struct Bundle {
    dir: &'static str,
    docs: Vec<(&'static str, &'static str, &'static str)>,
}

/// The in-memory [`BundleStore`]: a set of bundles, plain (bundle-less)
/// directories the recursive search can walk, and directories whose probe
/// is refused outright (the final-refusal path).
#[derive(Default)]
struct FixtureStore {
    bundles: Vec<Bundle>,
    dirs: Vec<String>,
    denied: Vec<&'static str>,
}

impl FixtureStore {
    fn with_ps() -> Self {
        FixtureStore {
            bundles: alloc::vec![Bundle {
                dir: "/System/Apps/ps.app",
                docs: alloc::vec![("en-US", "ps.md", PS_DOC), ("fr-FR", "ps.md", PS_DOC_FR),],
            }],
            dirs: Vec::new(),
            denied: Vec::new(),
        }
    }

    fn bundle(&self, dir: &str) -> Option<&Bundle> {
        self.bundles.iter().find(|bundle| bundle.dir == dir)
    }

    /// Every path the fixture knows: bundle directories plus the plain
    /// directories, from whose spellings `subdirs` derives the tree.
    fn paths(&self) -> impl Iterator<Item = &str> {
        self.bundles
            .iter()
            .map(|bundle| bundle.dir)
            .chain(self.dirs.iter().map(String::as_str))
    }
}

impl BundleStore for FixtureStore {
    fn bundle_exists(&self, bundle_dir: &str) -> Result<bool, Errno> {
        if self.denied.contains(&bundle_dir) {
            return Err(Errno::PermissionDenied);
        }
        Ok(self.bundle(bundle_dir).is_some())
    }

    fn locale_dirs(&self, bundle_dir: &str) -> Result<Vec<String>, Errno> {
        let Some(bundle) = self.bundle(bundle_dir) else {
            return Ok(Vec::new());
        };
        let mut dirs: Vec<String> = Vec::new();
        for (locale, _, _) in &bundle.docs {
            if !dirs.iter().any(|dir| dir == locale) {
                dirs.push((*locale).to_string());
            }
        }
        Ok(dirs)
    }

    fn subdirs(&self, dir: &str) -> Result<Vec<String>, Errno> {
        if self.denied.contains(&dir) {
            return Err(Errno::PermissionDenied);
        }
        let prefix = alloc::format!("{dir}/");
        let mut names: Vec<String> = Vec::new();
        for path in self.paths() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let name = rest.split('/').next().unwrap_or(rest);
            if !name.is_empty() && !names.iter().any(|seen| seen == name) {
                names.push(String::from(name));
            }
        }
        Ok(names)
    }

    fn read_doc(
        &self,
        bundle_dir: &str,
        locale_dir: &str,
        file_name: &str,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, Errno> {
        let Some(bundle) = self.bundle(bundle_dir) else {
            return Ok(None);
        };
        for (locale, name, text) in &bundle.docs {
            if *locale == locale_dir && *name == file_name {
                let bytes = text.as_bytes();
                let take = bytes.len().min(limit + 1);
                return Ok(Some(bytes[..take].to_vec()));
            }
        }
        Ok(None)
    }
}

/// The in-memory [`Console`]: captured output, captured fd-3 records, a
/// configurable row count, and a scripted key queue.
struct FixtureConsole {
    out: RefCell<Vec<u8>>,
    info: RefCell<Vec<Vec<u8>>>,
    rows: Option<u16>,
    keys: RefCell<Vec<u8>>,
}

impl FixtureConsole {
    /// A non-interactive console (no row count): the page streams whole.
    fn stream() -> Self {
        FixtureConsole {
            out: RefCell::new(Vec::new()),
            info: RefCell::new(Vec::new()),
            rows: None,
            keys: RefCell::new(Vec::new()),
        }
    }

    /// An interactive console `rows` high whose user will press `keys` in
    /// order (the queue's front is `keys[0]`).
    fn interactive(rows: u16, keys: &[u8]) -> Self {
        let mut queue = keys.to_vec();
        queue.reverse();
        FixtureConsole {
            out: RefCell::new(Vec::new()),
            info: RefCell::new(Vec::new()),
            rows: Some(rows),
            keys: RefCell::new(queue),
        }
    }

    fn output(&self) -> String {
        String::from_utf8(self.out.borrow().clone()).unwrap_or_default()
    }

    fn records(&self) -> Vec<String> {
        self.info
            .borrow()
            .iter()
            .map(|record| String::from_utf8(record.clone()).unwrap_or_default())
            .collect()
    }
}

impl Console for FixtureConsole {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.out.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }

    fn info(&self, record: &[u8]) {
        self.info.borrow_mut().push(record.to_vec());
    }

    fn rows(&self) -> Option<u16> {
        self.rows
    }

    fn read_key(&self) -> Result<Option<u8>, Errno> {
        Ok(self.keys.borrow_mut().pop())
    }
}

fn page(word: &str) -> Command {
    Command::Page {
        word: String::from(word),
        topic: None,
    }
}

fn topic(word: &str, topic: &str) -> Command {
    Command::Page {
        word: String::from(word),
        topic: Some(String::from(topic)),
    }
}

#[test]
fn renders_the_default_document_for_a_store_command() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    run(&page("ps"), &Request::default(), &store, &console).expect("page renders");
    let out = console.output();
    assert!(out.contains("NAME"), "heading missing: {out}");
    assert!(out.contains("list processes"), "body missing: {out}");
    assert!(console.records().is_empty(), "no advisory expected");
}

#[test]
fn an_unknown_word_is_command_not_found() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let err = run(&page("nope"), &Request::default(), &store, &console).unwrap_err();
    assert_eq!(err, ManError::CommandNotFound(String::from("nope")));
}

#[test]
fn the_store_bundle_shadows_a_path_bundle_of_the_same_name() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/Users/eve/tools/ps.app",
        docs: alloc::vec![(
            "en-US",
            "ps.md",
            "## NAME\n\nps — evil twin\n\n## SYNOPSIS\n\n`x`\n\n## DESCRIPTION\n\nNot this one.\n",
        )],
    });
    let console = FixtureConsole::stream();
    let request = Request {
        locale: None,
        path: Some("/Users/eve/tools"),
        home: None,
    };
    run(&page("ps"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("list processes"));
    assert!(!console.output().contains("evil twin"));
}

#[test]
fn a_path_bundle_serves_a_word_the_store_lacks() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/Users/root/tools/mine.app",
        docs: alloc::vec![(
            "en-US",
            "mine.md",
            "## NAME\n\nmine — my tool\n\n## SYNOPSIS\n\n`mine`\n\n## DESCRIPTION\n\nMine.\n",
        )],
    });
    let console = FixtureConsole::stream();
    let request = Request {
        locale: None,
        path: Some("/Users/root/tools"),
        home: None,
    };
    run(&page("mine"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("my tool"));
}

#[test]
fn a_final_refusal_stops_the_probe_rather_than_skipping_it() {
    let mut store = FixtureStore::with_ps();
    store.denied.push("/System/Apps/hidden.app");
    store.bundles.push(Bundle {
        dir: "/Users/root/tools/hidden.app",
        docs: alloc::vec![(
            "en-US",
            "hidden.md",
            "## NAME\n\nhidden\n\n## SYNOPSIS\n\n`x`\n\n## DESCRIPTION\n\nNever shown.\n",
        )],
    });
    let console = FixtureConsole::stream();
    let request = Request {
        locale: None,
        path: Some("/Users/root/tools"),
        home: None,
    };
    let err = run(&page("hidden"), &request, &store, &console).unwrap_err();
    assert_eq!(err, ManError::Store(Errno::PermissionDenied));
    assert!(console.output().is_empty());
}

#[test]
fn an_explicit_bare_path_names_no_bundle() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let err = run(
        &page("/System/Apps/ps.app/Run"),
        &Request::default(),
        &store,
        &console,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ManError::NotABundle(String::from("/System/Apps/ps.app/Run"))
    );
}

#[test]
fn a_bundle_word_opens_the_same_page_as_the_bare_command() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    run(&page("ps.app"), &Request::default(), &store, &console).expect("page renders");
    assert!(console.output().contains("list processes"));
}

#[test]
fn an_explicit_bundle_path_opens_its_own_page() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    run(
        &page("/System/Apps/ps.app"),
        &Request::default(),
        &store,
        &console,
    )
    .expect("page renders");
    assert!(console.output().contains("list processes"));
}

#[test]
fn a_topic_selects_its_own_document_within_the_bundle() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/System/Apps/top.app",
        docs: alloc::vec![
            (
                "en-US",
                "top.md",
                "## NAME\n\ntop — watch processes\n\n## SYNOPSIS\n\n`top`\n\n## DESCRIPTION\n\nWatches.\n",
            ),
            ("en-US", "keys.md", TOP_KEYS_DOC),
        ],
    });
    let console = FixtureConsole::stream();
    run(&topic("top", "keys"), &Request::default(), &store, &console).expect("topic renders");
    assert!(console.output().contains("interactive keys"));
}

#[test]
fn a_missing_document_is_the_clean_no_help_outcome() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let err = run(
        &topic("ps", "missing"),
        &Request::default(),
        &store,
        &console,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ManError::NoHelp {
            word: String::from("ps"),
            name: String::from("missing"),
        }
    );
}

#[test]
fn an_exact_locale_serves_its_translation_with_no_advisory() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let request = Request {
        locale: Some("fr-FR"),
        path: None,
        home: None,
    };
    run(&page("ps"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("lister les processus"));
    assert!(
        console.records().is_empty(),
        "exact locale needs no advisory"
    );
}

#[test]
fn a_locale_fallback_serves_default_and_emits_the_advisory() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let request = Request {
        locale: Some("de-DE"),
        path: None,
        home: None,
    };
    run(&page("ps"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("list processes"));
    let records = console.records();
    assert_eq!(records.len(), 1, "one advisory expected: {records:?}");
    let record = &records[0];
    assert!(record.contains("\"kind\":\"context\""), "{record}");
    assert!(record.contains("help.locale_fallback"), "{record}");
    assert!(record.contains("\"requested\":\"de-DE\""), "{record}");
    assert!(record.contains("\"served\":\"en-US\""), "{record}");
    assert!(record.ends_with('\n'), "JSONL framing: {record}");
}

#[test]
fn a_malformed_locale_preference_degrades_to_default_silently() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let request = Request {
        locale: Some("not a tag"),
        path: None,
        home: None,
    };
    run(&page("ps"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("list processes"));
    assert!(console.records().is_empty(), "default request, no advisory");
}

/// A moose of a fixture: the bundle the recursive-search tests file away in
/// nested folders.
const MOOSE_DOC: &str = "## NAME\n\nmoose — a filed-away app\n\n## SYNOPSIS\n\n`moose`\n\n## DESCRIPTION\n\nFound by the recursive search.\n";

#[test]
fn a_nested_apps_bundle_is_found_by_the_recursive_search() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/Apps/somefolder/anotherfolder/moose.app",
        docs: alloc::vec![("en-US", "moose.md", MOOSE_DOC)],
    });
    let console = FixtureConsole::stream();
    run(&page("moose"), &Request::default(), &store, &console).expect("page renders");
    assert!(console.output().contains("filed-away app"));
}

#[test]
fn the_users_own_apps_folder_is_searched_after_the_shared_store() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/Users/ada/Apps/somefolder/moose.app",
        docs: alloc::vec![("en-US", "moose.md", MOOSE_DOC)],
    });
    let console = FixtureConsole::stream();
    let request = Request {
        locale: None,
        path: None,
        home: Some("/Users/ada"),
    };
    run(&page("moose"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("filed-away app"));

    // Without a HOME there is no per-user root to search.
    let console = FixtureConsole::stream();
    let err = run(&page("moose"), &Request::default(), &store, &console).unwrap_err();
    assert_eq!(err, ManError::CommandNotFound(String::from("moose")));
}

#[test]
fn a_shared_store_match_wins_over_the_users_own() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/Apps/moose.app",
        docs: alloc::vec![("en-US", "moose.md", MOOSE_DOC)],
    });
    store.bundles.push(Bundle {
        dir: "/Users/ada/Apps/moose.app",
        docs: alloc::vec![(
            "en-US",
            "moose.md",
            "## NAME\n\nmoose — the private copy\n\n## SYNOPSIS\n\n`x`\n\n## DESCRIPTION\n\nNot this one.\n",
        )],
    });
    let console = FixtureConsole::stream();
    let request = Request {
        locale: None,
        path: None,
        home: Some("/Users/ada"),
    };
    run(&page("moose"), &request, &store, &console).expect("page renders");
    assert!(console.output().contains("filed-away app"));
    assert!(!console.output().contains("private copy"));
}

#[test]
fn a_shallower_bundle_beats_a_deeper_lexicographically_earlier_one() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/Apps/aaa/deep/moose.app",
        docs: alloc::vec![(
            "en-US",
            "moose.md",
            "## NAME\n\nmoose — the deep copy\n\n## SYNOPSIS\n\n`x`\n\n## DESCRIPTION\n\nNot this one.\n",
        )],
    });
    store.bundles.push(Bundle {
        dir: "/Apps/zzz/moose.app",
        docs: alloc::vec![("en-US", "moose.md", MOOSE_DOC)],
    });
    let console = FixtureConsole::stream();
    run(&page("moose"), &Request::default(), &store, &console).expect("page renders");
    assert!(console.output().contains("filed-away app"));
    assert!(!console.output().contains("deep copy"));
}

#[test]
fn the_search_never_descends_into_another_bundle() {
    let mut store = FixtureStore::with_ps();
    // A bundle filed *inside* another bundle's directory is not installed;
    // a bundle is a sealed unit, not a container of further apps.
    store.bundles.push(Bundle {
        dir: "/Apps/outer.app",
        docs: alloc::vec![(
            "en-US",
            "outer.md",
            "## NAME\n\nouter — a bundle\n\n## SYNOPSIS\n\n`x`\n\n## DESCRIPTION\n\nOuter.\n",
        )],
    });
    store.bundles.push(Bundle {
        dir: "/Apps/outer.app/Code/inner.app",
        docs: alloc::vec![("en-US", "inner.md", MOOSE_DOC)],
    });
    let console = FixtureConsole::stream();
    let err = run(&page("inner"), &Request::default(), &store, &console).unwrap_err();
    assert_eq!(err, ManError::CommandNotFound(String::from("inner")));
}

#[test]
fn an_exhausted_search_budget_is_reported_not_swallowed() {
    let mut store = FixtureStore::with_ps();
    // More walkable directories than the whole-invocation budget: the
    // truncation must surface as its own error, never as "not found".
    for i in 0..5000usize {
        store.dirs.push(alloc::format!("/Apps/d{i:04}"));
    }
    let console = FixtureConsole::stream();
    let err = run(&page("moose"), &Request::default(), &store, &console).unwrap_err();
    assert_eq!(
        err,
        ManError::SearchTruncated {
            root: String::from("/Apps"),
        }
    );
}

#[test]
fn the_pager_prompts_per_screenful_and_quits_on_q() {
    let store = FixtureStore::with_ps();
    // 3 rows → 2 page lines; the rendered document is longer, so the first
    // prompt appears after two lines and `q` stops the page there.
    let console = FixtureConsole::interactive(3, b"q");
    run(&page("ps"), &Request::default(), &store, &console).expect("page renders");
    let out = console.output();
    assert!(out.contains("--More--"), "prompt missing: {out}");
    assert!(
        !out.contains("DESCRIPTION"),
        "q must stop before the body: {out}"
    );
}

#[test]
fn the_pager_continues_on_space_and_advances_one_line_on_return() {
    let store = FixtureStore::with_ps();
    // Enough presses to reach the end of the short fixture page however the
    // renderer wraps it: spaces and returns must both make progress.
    let console = FixtureConsole::interactive(3, b" \r \r \r \r \r \r \r \r ");
    run(&page("ps"), &Request::default(), &store, &console).expect("page renders");
    let out = console.output();
    assert!(
        out.contains("DESCRIPTION"),
        "page must reach the body: {out}"
    );
}

#[test]
fn exhausted_pager_input_streams_the_remainder() {
    let store = FixtureStore::with_ps();
    // No keys at all: the first prompt sees end-of-input and the rest of
    // the page streams unprompted rather than hanging or failing.
    let console = FixtureConsole::interactive(3, b"");
    run(&page("ps"), &Request::default(), &store, &console).expect("page renders");
    assert!(console.output().contains("DESCRIPTION"));
}

#[test]
fn a_non_interactive_console_streams_without_prompting() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    run(&page("ps"), &Request::default(), &store, &console).expect("page renders");
    let out = console.output();
    assert!(!out.contains("--More--"), "no prompt when streaming: {out}");
    assert!(out.contains("DESCRIPTION"));
}

#[test]
fn short_help_renders_mans_own_document() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/System/Apps/man.app",
        docs: alloc::vec![("en-US", "man.md", MAN_DOC)],
    });
    let console = FixtureConsole::stream();
    run(&Command::ShortHelp, &Request::default(), &store, &console).expect("short help renders");
    let out = console.output();
    assert!(out.contains("show a command's help"), "{out}");
    assert!(
        !out.contains("Renders the Help document"),
        "short view must omit the description body: {out}"
    );
}

#[test]
fn short_help_falls_back_to_the_usage_banner_without_its_bundle() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    run(&Command::ShortHelp, &Request::default(), &store, &console).expect("usage banner");
    assert!(console.output().contains(USAGE));
}

/// A hostile renderer: frames terminal-control noise as its render reply,
/// exactly as a compromised worker process could.
struct EvilRenderer;

impl Service for EvilRenderer {
    fn handle(&mut self, _request: &[u8]) -> Vec<u8> {
        // REPLY_RENDER (tag 1) framing a screen-clear escape: the client's
        // whitelist must refuse it whole.
        let payload = b"safe\x1b[2Jtext";
        let mut reply = alloc::vec![1u8];
        reply.extend_from_slice(&u32::try_from(payload.len()).expect("short").to_le_bytes());
        reply.extend_from_slice(payload);
        reply
    }
}

#[test]
fn a_hostile_renderer_withholds_the_page_and_reports_it() {
    let store = FixtureStore::with_ps();
    let console = FixtureConsole::stream();
    let mut sandbox = ParserSandbox::new(LoopbackLauncher::new(|| EvilRenderer), NullSink);
    let err = crate::client::run(
        &page("ps"),
        &Request::default(),
        &store,
        &console,
        &mut sandbox,
    )
    .unwrap_err();
    assert_eq!(err, ManError::Render(HelpRenderFailure::ReplyMalformed));
    assert!(
        console.output().is_empty(),
        "no byte of a disbelieved render may reach the console"
    );
}

#[test]
fn short_help_degrades_to_the_usage_banner_when_the_renderer_is_hostile() {
    let mut store = FixtureStore::with_ps();
    store.bundles.push(Bundle {
        dir: "/System/Apps/man.app",
        docs: alloc::vec![("en-US", "man.md", MAN_DOC)],
    });
    let console = FixtureConsole::stream();
    let mut sandbox = ParserSandbox::new(LoopbackLauncher::new(|| EvilRenderer), NullSink);
    crate::client::run(
        &Command::ShortHelp,
        &Request::default(),
        &store,
        &console,
        &mut sandbox,
    )
    .expect("degrades to the usage banner");
    assert!(console.output().contains(USAGE));
}
