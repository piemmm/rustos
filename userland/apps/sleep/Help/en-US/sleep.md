## NAME

sleep — pause for a sum of time intervals

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Pauses for the sum of the given intervals and then exits.

Each `NUMBER` is a floating-point value; an optional single-letter
`SUFFIX` scales it: `s` for seconds (the default), `m` for minutes, `h`
for hours, and `d` for days. Several operands are added together, so
`sleep 1m 30s` pauses for ninety seconds. `inf` (or `infinity`) pauses
until the process is killed.

Unlike a shell's own timing, `sleep` sleeps off the processor: the task is
parked until the interval elapses and never spins a core.

A negative value, a `nan`, an unknown suffix, or extra characters after the
number is an `invalid time interval`. Giving no operand at all is a
`missing operand`.

This command does not print an OS version; TAIRiX has no such string, so —
unlike GNU `sleep` — it has no `--version` option.

## OPTIONS

- `-h, -?` — show this command's own short help.
- `--` — end option parsing; any later argument is an operand.

## EXAMPLES

- `sleep 5` — pause for five seconds.
- `sleep 1.5h` — pause for ninety minutes.
- `sleep 1m 30s` — pause for ninety seconds (the operands are summed).
- `sleep inf` — pause until the process is killed.

## EXIT STATUS

- `0` — the interval elapsed, or a requested short help was written.
- `1` — writing the short help failed.
- `2` — the command line was not understood (an unknown option, a missing
  operand, or an invalid time interval).

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `top`
