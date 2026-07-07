## NAME

reset — adfer y derfynell i gyflwr call

## SYNOPSIS

`reset`

## DESCRIPTION

Mae'n dadwneud y cyflwr y gall rhaglen sgrin-lawn a chwalodd ei adael
ar ei hôl. Yn gyntaf adferir y ddisgyblaeth fewnbwn i'r rhagosodiad
rhyngweithiol (mae nodau a deipir yn atseinio eto). Yna ysgrifennir y
dilyniant adfer: gadael y sgrin amgen, dangos y cyrchwr, ailosod
lliwiau a phriodoleddau, ailosod y rhanbarth sgrolio, ac yn olaf symud
y cyrchwr adref a dileu'r arddangosfa.

Y derfynell a enwir yn `TERM` sy'n penderfynu pa weithrediadau a
ysgrifennir; hepgorir gweithrediad nad yw'r derfynell yn ei ddeall.
Dim ond adferiad y ddisgyblaeth fewnbwn a gaiff terfynell heb unrhyw
reolaethau o gwbl (mae `TERM` anhysbys yn dirywio i'r sylfaen «dumb»).

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `reset` — adfer y derfynell wedi i raglen sgrin-lawn chwalu.

## EXIT STATUS

- `0` — adferwyd y derfynell.
- `1` — ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `TERM` — y derfynell yr ysgrifennir ei dilyniant adfer.
- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `clear`
- `man`
