## NAME

whoami — argraffu enw cyfrif y defnyddiwr cyfredol

## SYNOPSIS

`whoami`

## DESCRIPTION

Mae'n argraffu'r enw defnyddiwr sy'n gysylltiedig â hunaniaeth y broses
hon, ac yna nod llinell newydd — a dim byd arall.

Nid oes gan RustOS `/etc/passwd`: daw'r dynodwr defnyddiwr o'r cofnod
y mae'r cnewyllyn yn ei gadw am y broses sy'n galw, a daw enw'r cyfrif
cyfatebol o gyfeiriadur cyhoeddus y cyfrifon yn yr API gwybodaeth
system. Os nad yw'r cyfeiriadur yn cynnwys enw ar gyfer y dynodwr,
mae'r gorchymyn yn adrodd `cannot find name for user ID <uid>` ac yn
methu.

Nid yw'r gorchymyn yn derbyn operandau; mae ymresymiad yn wall
`extra operand`.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.
- `--` — gorffen dosrannu opsiynau; mae pob ymresymiad diweddarach yn
  dal i fod yn operand dros ben (nid yw `whoami` yn derbyn yr un).

## EXAMPLES

- `whoami` — argraffu enw'r cyfrif sy'n rhedeg y gorchymyn.

## EXIT STATUS

- `0` — ysgrifennwyd yr enw (neu'r cymorth byr y gofynnwyd amdano).
- `1` — methodd darllen yr hunaniaeth, ymholiad y cyfeiriadur neu'r
  allbwn, neu nid yw'r cyfeiriadur yn cynnwys enw ar gyfer y dynodwr
  defnyddiwr.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `users`
- `ps`
