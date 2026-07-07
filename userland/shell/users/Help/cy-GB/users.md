## NAME

users — gweinyddu cyfrifon defnyddwyr a grwpiau

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Mae'n rhedeg y sesiwn ryngweithiol gweinyddu cyfrifon dros y
rhyngwyneb gwarchodedig `users_admin`. Penderfynir pob gweithred ar
ochr y cnewyllyn o dan eich hunaniaeth a ardystiwyd gan y cnewyllyn:
heb `CAP_USER_ADMIN` yn nenfwd eich cyfrif gwrthodir pob gweithred wrth
y dosbarthu. Darllenir cyfrineiriau gyda'r atsain terfynell i ffwrdd
a'u stwnsio ar ochr y cleient yn gofnod halltiedig; nid yw'r testun
plaen byth yn croesi'r rhyngwyneb ac ni chaiff byth ei atseinio na'i
gofnodi.

Nid yw'r offeryn yn cymryd operandau: gweinyddir cyfrifon â
gorchmynion a deipir o fewn y sesiwn.

- `list` — rhestru cyfrifon defnyddwyr.
- `groups` — rhestru grwpiau.
- `create <name> <uid> <gid>` — creu cyfrif.
- `passwd <name>` — gosod cyfrinair cyfrif.
- `lock <name>`, `unlock <name>` — analluogi neu ail-alluogi cyfrif.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — golygu
  grantiau gallu cyfrif.
- `deluser <name>` — dileu cyfrif.
- `addgroup`, `delgroup` — creu neu ddileu grŵp.
- `help` — rhestru gorchmynion y sesiwn.
- `exit`, `quit` — gorffen y sesiwn.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael.

## EXIT STATUS

- `0` — daeth y sesiwn i ben yn lân, neu dangoswyd y cymorth byr.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `man`
