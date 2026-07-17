## NAME

false — no hacer nada, sin éxito

## SYNOPSIS

`false [argumentos ignorados]`

## DESCRIPTION

Termina con el estado `1`, ignorando todos los argumentos. Los guiones
lo usan allí donde se necesita una orden que siempre falla: como
condición siempre falsa o fallo deliberado.

Solo se atiende un **primer** argumento `-h`, `-?` o `--help` (la
posición en la que GNU `false` atiende `--help`); en cualquier posición
posterior esas palabras se ignoran como todo lo demás. A diferencia de
GNU `false --help`, que aun así termina con `1`, aquí una ayuda breve
servida termina con `0`: la convención de ayuda breve de TAIRiX.

## OPTIONS

- `-h, -?` — (solo como primer argumento) mostrar la ayuda breve de
  esta orden.

## EXAMPLES

- `false` — fallar.
- `until false; do …; done` — ejecutar el cuerpo una vez (la condición
  es siempre falsa).

## EXIT STATUS

- `1` — siempre (la única finalidad de la herramienta).
- `0` — se sirvió la ayuda breve solicitada.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `true`
- `man`
