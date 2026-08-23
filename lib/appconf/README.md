# tairix-appconf

Stability tier: **experimental**.

The per-app configuration document engine: the one definition of the
`key = value` settings format the app-data store serves (`plans/APPDATA.md`) —
the dotted key grammar, the bare and quoted value forms with their escapes,
the typed accessors, the fixed fail-closed bounds, and a rewrite that
preserves what a human wrote.

The engine does **no I/O**. A document is text in and text out, so the same
code serves the app-data daemon, a client library, and a host test, and every
permission check stays where it belongs.

## The hard requirement

A rewrite never destroys what a human wrote. A document is modelled as an
ordered list of *lines*, not as a map: `set` rewrites the one line it must —
keeping any inline comment on it — and leaves every other byte exactly as it
found it, including comments, blank lines, the user's own alignment, key
order, and lines the grammar refused. An app that saves its settings cannot
silently destroy a hand edit.

## Tolerance, and where it stops

A line the grammar cannot read is retained verbatim and reported by
`unparsed()`; it never aborts the read, so one fumbled line cannot cost a user
every other setting. That tolerance is confined to line *content*: the
document-level bounds (`MAX_DOCUMENT_LEN`, `MAX_LINES`, `MAX_SETTINGS`) are
fixed security bounds on untrusted input and fail closed with a typed
`ConfError`, never by truncating a hostile store into a document that means
something else. `MAX_KEY_LEN`, `MAX_KEY_DEPTH`, and `MAX_VALUE_LEN` bound one
line's work and make an over-long line *unparsed* rather than a setting.

Fractions are permille integers, not decimals: a permille round-trips through
text exactly, needs no float parser in a `no_std` build, and is already how
the shipped effect strengths are expressed.

## A document may hold secrets

The app-data store's **sealed** scope is a document of this format, so the engine
treats a line's bytes as secret unconditionally: every line it discards — an
overwritten setting, a collapsed duplicate, an `unset` removal — and every line of
a document that goes out of scope is wiped before it is freed, through the
audited `zeroize`. That lives here rather than in the callers so no discard path
can forget it, and `Document` implements no `Debug`, so it cannot reach a log by
construction. The one copy the engine does not own is `render`'s return value,
and its rustdoc says so.

## Relationship to the other line-oriented stores

`lib/sysconfig`, `lib/netconfig`, and init's service registry read a
*space*-separated `key value` line against a closed key registry and refuse a
whole document that deviates — the right semantics for a store the system
boots from, and deliberately **not** folded in here. This engine differs on
both counts: an open key namespace the app owns, and per-line tolerance. It
also cannot share their comment tokenisation (`tairix_util::conf::strip_comment`
cuts at the first `#` unconditionally), because here a `#` inside a quoted
value is a literal; the tokenisation has to know about quoting, so it lives
with the grammar that has quotes.

Fuzzed by `lib/appconf/tests/fuzz_appconf.rs` (`cargo xtask fuzz`), which
holds the parse/render fixed point and the one-key-per-write property over
generated and arbitrary input.
