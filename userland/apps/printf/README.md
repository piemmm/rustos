# `tairix-printf` — format and print data

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Commands/printf.app/` so the shell resolves the bare word
`printf` to it. `printf` is the GNU coreutils tool: print ARGUMENTs under
the control of FORMAT, a template of literal text, backslash escapes
(`\n`, `\t`, `\NNN`, `\xHH`, `\uHHHH`, `\UHHHHHHHH`, and `\c`, which ends
all output), and `%` conversion directives — `diouxX` integers, `eEfFgGaA`
floats, `%c`, `%s`, `%b` (a string whose own escapes are interpreted, with
`\0NNN` octal), `%q` (a string quoted for shell reuse), and `%%` — with the
C flags (`-+ #0'`), field width, and precision, both settable to `*`. The
FORMAT is reused until every ARGUMENT is consumed, exactly as in GNU
`printf`: a missing argument converts as zero or the empty string, an
invalid or partially numeric argument is diagnosed on standard error and
converted as far as it goes (the run continues and exits `1`), and an
invalid conversion specification (an unknown conversion letter, or a
flag/width/precision on a conversion that does not accept it, e.g. any on
`%b`/`%q`) is fatal. A leading `'`/`"` on a numeric argument converts the
next character's code point. `-h`/`-?`/`--help` as the first argument
render the tool's own short help from its bundled `Help/` tree through the
shared `lib/help` engine, in the locale the inherited `LANG` variable
names, falling back to the usage banner when the tree is unavailable; `--`
before FORMAT ends option scanning as in GNU.

Deliberate platform divergences, both documented in the bundle's help:

- GNU `printf` computes floating-point conversions in C `long double`;
  TAIRiX computes in IEEE 754 `f64` (`double`), through the same shared
  C-locale renderer as `seq -f` (`tairix_util::cfloat`). A value beyond
  `double`'s range therefore renders as `inf` where a glibc build prints
  the `long double` value, and `%a`'s exact spelling is the `double` one.
- `-h`/`-?`/`--help` as the first argument serve the TAIRiX short-help
  convention (plans/APPS.md §4); GNU `printf` would treat `-h`/`-?` as
  FORMAT. Spell such a format `printf -- -h...`.
- A numeric character constant (`'x`) converts the argument's first
  *character* (its Unicode scalar value — TAIRiX argument vectors are
  UTF-8); glibc's conversion is locale-dependent (bytewise in the C
  locale).

Everything else — option grammar, conversion semantics, diagnostics
wording, and exit statuses (`0` success; `1` for a conversion diagnostic,
a fatal specification error, or a missing FORMAT) — follows GNU `printf`
(`AGENTS.md` §16.7), pinned by the unit tests against the observed
behaviour of GNU coreutils `printf`.

The crate is the pure `no_std` engine (template walk, escape and
directive rendering, C-locale argument conversion over
`tairix_util::cnum`) behind injected `Output` seams, plus the freestanding
`Run` binary. Its `Help/` tree is authored on disk in this bundle and read
back at runtime through the shared `HelpSource` seam — never embedded in
the binary (plans/APPS.md §6.1).
