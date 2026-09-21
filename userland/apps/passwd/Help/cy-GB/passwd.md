## NAME

passwd — gosod cyfrinair cyfrif

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Mae'n disodli cyfrinair cadwedig y cyfrif a enwir. Gweithred weinyddol yw
gosod cyfrinair: mae'r gronfa'n gwrthod galwr heb allu gweinyddu defnyddwyr.

Nid yw cyfrinair clir byth yn croesi'r alwad system. Mae'r offeryn yn gofyn
ddwywaith gyda'r adlais wedi'i ddiffodd, yn stwnsio'r hyn a deipiwyd yn
gofnod PBKDF2 hallt — a'r halen yn dod o ffynhonnell hap y cnewyllyn — ac yn
anfon y cofnod; sero'r ddau glustog clir cyn gynted ag y bo'r cofnod yn
bodoli.

Mae angen enw'r cyfrif. Mae `passwd` GNU heb operand yn newid cyfrinair y
galwr ei hun, a byddai hynny ar TAIRiX yn gofyn am lwybr hunanwasanaeth
difraint nad yw'n bodoli: mae'r rhyngwyneb gweinyddu cyfrifon cyfan wedi'i
warchod gan allu, a byddai naddu «dy gofnod dy hun» ohono yn newid i'r model
diogelwch, nid yn gyfleustra.

Mae galwr heb derfynell — rhaglen graffigol, y mae ei mewnbwn safonol ar gau
dan y brocer dyrchafu — yn stwnsio'r cyfrinair ei hun ac yn trosglwyddo'r
cofnod gorffenedig gyda `--record`, fel nad oes clir ar y naill ochr na'r
llall. Gwirir bod y cofnod yn dda ei ffurf cyn ei storio.

Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach yn operand.

## OPTIONS

- `--record RECORD` — cofnod PBKDF2 hallt parod, ar gyfer galwr heb derfynell i ofyn arni.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `passwd ada` — gofyn ddwywaith a gosod cyfrinair y cyfrif.

## EXIT STATUS

- `0` — disodlwyd y cyfrinair.
- `1` — gwrthododd neu fethodd y gronfa'r disodli, roedd y ddau nodiad yn
  anghytuno, nid oedd hap ar gael, neu roedd y cofnod ar ffurf wallus;
  argraffir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel `cy-GB`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
