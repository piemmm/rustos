## NAME

groupdel — dileu grŵp

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Mae'n tynnu un grŵp o gofrestr y grwpiau. Gweithred weinyddol yw dileu grŵp: mae'r gofrestr yn gwrthod galwr heb allu gweinyddu defnyddwyr.

Y gofrestr yw'r awdurdod ar yr hyn y gellir ei dynnu. Mae'n gwrthod dileu grŵp y mae cyfrif yn dal i gyfeirio ato, fel na fydd unrhyw gyfrif yn enwi grŵp nad yw'n bodoli.

Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach yn operand.

## OPTIONS

- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `groupdel staff` — dileu'r grŵp `staff`.

## EXIT STATUS

- `0` — dilëwyd y grŵp.
- `1` — gwrthododd neu fethodd y gofrestr y dileu (er enghraifft gallu coll, grŵp anhysbys, neu grŵp y cyfeirir ato o hyd); argraffir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel `cy-GB`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
