## NAME

reset — restore the terminal to a sane state

## SYNOPSIS

`reset`

## DESCRIPTION

Undoes the state a crashed full-screen program can leave behind.
First the input discipline is restored to the interactive default
(typed characters echo again). Then the restoration sequence is
written: leave the alternate screen, show the cursor, reset colours
and attributes, reset the scroll region, and finally move the cursor
home and erase the display.

Which operations are written is decided by the terminal named in
`TERM`; an operation the terminal does not understand is omitted. A
terminal with no controls at all (an unknown `TERM` degrades to the
dumb baseline) gets only the input-discipline restore.

## OPTIONS

- `-h, -?` — show this command's own short help.

## EXAMPLES

- `reset` — restore the terminal after a full-screen program crashed.

## EXIT STATUS

- `0` — the terminal was restored.
- `1` — the output could not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `TERM` — the terminal whose restoration sequence is written.
- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `clear`
- `man`
