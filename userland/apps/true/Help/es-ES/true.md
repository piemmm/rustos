## NAME

true — no hacer nada, con éxito

## SYNOPSIS

`true [argumentos ignorados]`

## DESCRIPTION

Termina con el estado `0`, ignorando todos los argumentos. Los guiones
lo usan allí donde se necesita una orden que siempre tiene éxito: como
orden de relleno, condición siempre verdadera o cuerpo de un bucle.

Solo se atiende un **primer** argumento `-h`, `-?` o `--help` (la
posición en la que GNU `true` atiende `--help`); en cualquier posición
posterior esas palabras se ignoran como todo lo demás.

## OPTIONS

- `-h, -?` — (solo como primer argumento) mostrar la ayuda breve de
  esta orden.

## EXAMPLES

- `true` — terminar con éxito.
- `while true; do …; done` — repetir hasta la interrupción.

## EXIT STATUS

- `0` — siempre (la única finalidad de la herramienta).
- `1` — no se pudo escribir la ayuda breve solicitada.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `false`
- `man`
