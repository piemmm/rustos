# `tairix-tee` — read from standard input and write to standard output and files

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Commands/tee.app/` so the shell resolves the bare word
`tee` to it. `tee` is the GNU coreutils tool: it copies standard input to
standard output and to each file operand (created if absent; overwritten,
or appended with `-a`), so a pipeline's data can be seen and captured at
once. Option handling matches GNU: `-a`/`--append`, `-p`,
`--output-error[=MODE]` with the `warn`/`warn-nopipe`/`exit`/`exit-nopipe`
modes matched like GNU `argmatch` (an unambiguous prefix is accepted; the
value arrives only attached with `=`, and a bare `--output-error` selects
`warn-nopipe`), interleaved options and operands, and `--` end-of-options.
A `-` operand names a file called `-`, as in GNU. `-h`/`-?`/`--help`
render the tool's own short help from its bundled `Help/` tree through the
shared `lib/help` engine, in the locale the inherited `LANG` variable
names, falling back to the usage banner when the tree is unavailable.

Two deliberate, documented divergences follow from TAIRiX having no
`SIGPIPE` and no per-process signal disposition:

- The "pipe" class of the GNU modes maps to the standard-output copy —
  the one output of this tool that can be a pipe. A consumer going away
  surfaces as a write error there, never a signal: without
  `--output-error` it stops the run with the reason stated on standard
  error (the fail-loud analogue of GNU dying of `SIGPIPE`); under a
  `-nopipe` mode it is dropped silently without affecting the exit
  status.
- GNU `tee -i`/`--ignore-interrupts` is **staged**, not stubbed: there is
  no signal disposition to set today, so the switch is refused as
  unrecognised and arrives in the change that lands that kernel work
  (the `mkdir -m` precedent).

The engine streams in constant memory (one 4 KiB chunk fanned out to
every still-live output) and stops reading once no output remains. A
failed output is diagnosed and handled per the selected mode, exactly as
GNU `tee.c` nulls a failed descriptor; a failed diagnostic write is
fatal. The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `lib/abi` vocabulary and the shared `tairix-help` crate, so it
never links a kernel or driver crate. Its manifest (`AppInfo.toml`)
requests `CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` —
within the session baseline — and the secured VFS still authorises every
path per-inode under the caller's attested identity.

## Usage

```
tee [-ap] [--output-error[=mode]] [--] [file...]

  -a, --append           append to the files; do not overwrite
  -p                     same as --output-error=warn-nopipe
  --output-error[=mode]  warn | warn-nopipe | exit | exit-nopipe
  -h, -?                 show this command's own short help
  --                     end option parsing
```

## Exit status

- `0` — every output was served to end-of-input (or a requested short
  help was served); a standard-output failure tolerated by a `-nopipe`
  mode does not change this.
- `1` — an output failed in a way the selected mode counts, input could
  not be read, or a diagnostic could not be delivered.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
