# `tairix-seq` — print a sequence of numbers

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/seq.app/` so the shell resolves the bare word
`seq` to it. `seq` is the GNU coreutils tool: print the numbers from
FIRST to LAST in steps of INCREMENT (both defaulting to 1), with the GNU
option surface — `-f`/`--format` (a printf-style floating-point format
with one `%` directive of type `e`/`f`/`g`/`a`), `-s`/`--separator`, and
`-w`/`--equal-width` — and the GNU output rules: the default precision is
inferred from the operands' spellings, plain integer runs are generated
in exact decimal string arithmetic (arbitrarily large; `inf` as LAST is
permitted), and the floating-point path prints the value one step past
LAST when it renders equal to it. Option scanning matches GNU `seq`: no
permutation, a leading negative number is an operand, and `-f` may not
be combined with `-w`. `-h`/`-?`/`--help` render the tool's own short
help from its bundled `Help/` tree through the shared `lib/help` engine,
in the locale the inherited `LANG` variable names, falling back to the
usage banner when the tree is unavailable.

One deliberate platform divergence: GNU `seq`'s floating-point path
computes in C `long double`, TAIRiX computes in `f64` — only non-integer
sequences whose operands need more than 53 bits of significand can print
differently from a glibc build on x86; the exact decimal path is
unaffected. The printf-equivalent renderer implements C-locale `%e`,
`%f`, `%g`, and `%a` semantics (flags `-+#0 '`, width, precision) so a
`-f` format prints what C's `printf` prints **for a `double`**; the
visible consequence of the `long double` divergence is `%a`'s spelling —
`seq -f %a 1.5 1.5` prints `0x1.8p+0` here where glibc's `%La`
normalisation prints the same value as `0xcp-3`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its only dependency is
the shared `tairix-help` crate, so it never links a kernel or driver
crate. Its manifest (`AppInfo.toml`) requests `CAP_CONSOLE_WRITE` and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS still
authorises every path per-inode under the caller's attested identity.

## Usage

```
seq [-f format] [-s string] [-w] [first [increment]] last

  -f, --format <format>     printf-style floating-point format
  -s, --separator <string>  separator between numbers (default: \n)
  -w, --equal-width         pad to equal width with leading zeros
  -h, -?                    show this command's own short help
  --                        end option parsing
```

## Exit status

- `0` — the sequence (or a requested short help) was written.
- `1` — the output stopped accepting bytes.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
