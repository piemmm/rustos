# `tairix-appconf` — per-app configuration documents

`lib/appconf` (`tairix-appconf`) is the one definition of the `key = value`
settings format the per-app data store serves (`plans/APPDATA.md`). It parses
such a document, answers typed reads against it, rewrites single settings, and
renders it back — with **no I/O**, so the same code serves the app-data
daemon, a client library, and a host test, and every permission check stays
where it belongs.

## Why a line format, and why this one

The requirement is that a human can open the file in a text editor and that
bad formatting is survivable. A nested document format fails the second
outright — one misplaced delimiter loses the whole file — so the grammar is
one setting per line, and structure comes from dotted keys (`effects.blur`)
rather than from nesting syntax a hand edit can get wrong.

```text
# The user's own comment survives a save.
scheme       = dark
font.size    = 14
effects.blur = 500
recent.0     = /Users/ada/Documents/notes.txt
greeting     = "  leading space, a # sign, and \n an escape "
```

- **Keys** are dot-separated segments of ASCII lowercase letters, digits, `-`
  and `_`, each non-empty and starting with a letter or digit
  (`validate_key`). The grammar is narrow because keys are compared
  byte-for-byte: admitting case variants would let two spellings of "the same"
  setting disagree, and admitting whitespace or `=` would let a key swallow
  its own separator.
- **Values** are bare (whitespace-trimmed, ending at an unquoted `#`) or
  `"quoted"` with `\\`, `\"`, `\n`, and `\t` escapes — the form a value needs
  when it carries leading or trailing space, a `#`, a quote, or a newline. An
  unknown escape, an unterminated quote, or text after the closing quote makes
  the line unparsed rather than a guess at what was meant.
- **Fractions are permille integers**, not decimals: a permille round-trips
  through text exactly, needs no float parser in a `no_std` build, and is
  already how the shipped effect strengths are expressed.

## A rewrite never destroys what a human wrote

This is the engine's hard requirement, not a nicety. A document is modelled as
an ordered list of *lines*, not as a map: `set` rewrites the one line it must
— keeping any inline comment on that line — and leaves every other byte
exactly as it found it, including comments, blank lines, the user's own
alignment, key order, and lines the grammar refused. An app that saves its
settings therefore cannot silently destroy a hand edit. The fuzz harness holds
this as a property: a parsed document renders back byte-for-byte, and a write
touches one key and nothing else.

A duplicate key reads as the **last** one written, which is what appending a
line to a file means; a `set` then collapses the duplicates so the file says
once what it means.

## Tolerance, and where it stops

A line the grammar cannot read is retained verbatim and reported by
`unparsed()` (with its 1-based line number, so a caller can tell the user
*which* line), and it never aborts the read: one fumbled line cannot cost a
user every other setting.

That tolerance is confined to line *content*. The document-level bounds are
fixed security bounds on untrusted input (`AGENTS.md` §24.4) and fail closed
with a typed `ConfError`, never by truncating a hostile store into a document
that means something else:

| Bound | What it guards |
|---|---|
| `MAX_DOCUMENT_LEN` | the parse work a store can demand before a byte is believed — and the app-data reply body's own width, so it is imported from `lib/abi` rather than restated |
| `MAX_LINES` | the parser's allocation — a document of newlines is small in bytes and large in lines |
| `MAX_SETTINGS` | the key space one app can create |
| `MAX_KEY_LEN`, `MAX_KEY_DEPTH`, `MAX_VALUE_LEN` | one line's work; exceeding them makes the line unparsed, not a setting |

Typed reads distinguish the three answers a caller needs: `Ok(None)` (no such
setting), `Ok(Some(v))` (a value of the requested type), and
`Err(ValueMalformed)` (the setting is there but does not mean what was asked).
An app can therefore report a broken value instead of silently substituting a
default.

## One question, whichever document answers it

An application's registry is a fixed set of keys read out of *something*: its
own layered `tairix_appdata::Settings` handle, the `Document` another
application's published scope answered with, or one that arrived over a
channel. Those are different types with one question between them, so the
question is named once — the `Lookup` trait, implemented here for `Document`
and in the app-data client for its handle — and every registry is written once
against it. A loader per surface would be one more chance for two of them to
disagree about what a missing key means.

The grammar is also *inside* the one `lib/abi` store-name grammar's shape:
every bundle identifier and bulk-store name `validate_store_name` admits is a
legal key, which is what lets a registry key on an identifier
(`<bundle-id>.<field>`) with no risk of rendering a key no document can hold.
A unit test pins the containment in both directions.

## A document may hold secrets

The app-data store's **sealed** scope is a document of this format, so the
engine treats a line's bytes as secret unconditionally: every line it discards —
an overwritten setting, a collapsed duplicate, an `unset` removal — and every
line of a document that goes out of scope is wiped before it is freed, through
the audited `zeroize` (volatile writes the optimiser may not elide). That lives
in the engine rather than in its callers so no discard path can forget it, and
`Document` deliberately implements no `Debug` or `Display`, so a document cannot
reach a log or a panic message by construction.

The one copy the engine does not own is `render`'s return value; its rustdoc
says so, and the service wipes it once the document has been sealed or framed.

## Relationship to the other line-oriented stores

`lib/sysconfig`, `lib/netconfig`, and init's service registry read a
*space*-separated `key value` line against a closed key registry, and refuse a
whole document that deviates — the right semantics for a store the system
boots from, and deliberately **not** folded in here. This engine differs on
both counts: an open key namespace the app owns, and per-line tolerance. It
also cannot share their comment tokenisation (`tairix_util::conf::strip_comment`
cuts at the first `#` unconditionally), because here a `#` inside a quoted
value is a literal character; the tokenisation has to know about quoting, so it
lives with the grammar that has quotes.

The crate is `no_std` (with `alloc`) and depends on `lib/abi` — for the three
bounds a key, a value, and a document cross the app-data channel under — and on
the audited `zeroize`. It forbids `unsafe` and has no
`unwrap`/`expect`/`panic!` in production paths. Stability tier:
`experimental`.
