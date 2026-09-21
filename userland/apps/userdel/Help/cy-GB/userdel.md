## NAME

userdel — dileu cyfrif defnyddiwr

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Mae'n tynnu un cyfrif o gronfa'r defnyddwyr. Gweithred weinyddol yw dileu cyfrif: mae'r gronfa'n gwrthod galwr heb allu gweinyddu defnyddwyr.

Y gronfa yw'r awdurdod ar yr hyn y gellir ei dynnu. Mae'n gwrthod dileu'r cyfrif gweithredol olaf sy'n cael gweinyddu defnyddwyr, fel na all system byth aros heb ffordd i'w gweinyddu.

Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach yn operand.

## OPTIONS

- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `userdel ada` — dileu'r cyfrif `ada`.

## EXIT STATUS

- `0` — dilëwyd y cyfrif.
- `1` — gwrthododd neu fethodd y gronfa'r dileu (er enghraifft gallu coll, cyfrif anhysbys, neu'r gweinyddwr olaf); argraffir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel `cy-GB`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
