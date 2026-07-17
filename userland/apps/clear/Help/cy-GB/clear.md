## NAME

clear — clirio sgrin y derfynell

## SYNOPSIS

`clear [-x]`

## DESCRIPTION

Mae'n ysgrifennu'r dilyniant sy'n symud y cyrchwr i'r gornel chwith
uchaf ac yn dileu'r arddangosfa gyfan, gan adael sgrin wag. Y derfynell
a enwir yn `TERM` sy'n penderfynu pa ddilyniant a ysgrifennir; mae
terfynell na all glirio (mae `TERM` anhysbys yn dirywio i'r sylfaen
«dumb») yn gwneud i'r gorchymyn fethu yn hytrach nag argraffu beitiau y
byddai'r derfynell yn eu dangos fel sbwriel.

Nid yw consolau TAIRiX yn cadw ôl-sgrolio, felly nid oes ôl-sgrolio i'w
glirio: derbynnir `-x` (opsiwn GNU sy'n cadw'r ôl-sgrolio) er
cydnawsedd sgriptiau ac nid yw'n newid dim.

## OPTIONS

- `-x` — fe'i derbynnir er cydnawsedd GNU; nid yw consol TAIRiX yn cadw
  ôl-sgrolio, felly mae'r allbwn yn union yr un fath hebddo a gydag ef.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `clear` — clirio'r sgrin.

## EXIT STATUS

- `0` — ysgrifennwyd y dilyniant clirio.
- `1` — ni all y derfynell glirio, neu ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `TERM` — y derfynell yr ysgrifennir ei dilyniant clirio.
- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `reset`
- `man`
