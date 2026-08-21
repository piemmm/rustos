## NAME

unlink — eliminar un solo nombre

## SYNOPSIS

`unlink [--] archivo`

## DESCRIPTION

Elimina exactamente un nombre, mediante la única llamada al sistema de
archivos que nombra la función POSIX `unlink`. Deliberadamente no hay
recursión, ni forzado, ni confirmación, ni informes: un guion que deba
eliminar un solo nombre y nada más dispone de una herramienta incapaz de
hacer más. Use `rm` para esas opciones y `rmdir` para un directorio.

El nombre se elimina **tal como se escribe**. Un enlace simbólico se
elimina él mismo y nunca se sigue, de modo que un enlace colocado ahí no
puede redirigir la eliminación a su destino.

Un **directorio** lo rechaza el sistema de archivos, en el mismo
recorrido bloqueado que habría eliminado la entrada: aquí no existe
ninguna carrera entre comprobar y eliminar.

Se requiere exactamente un operando: ningún operando y dos o más
operandos son ambos errores de uso, y nada se elimina. `--` termina el
análisis de opciones, de modo que un nombre que empieza por guion sigue
siendo eliminable.

## OPTIONS

- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `unlink viejo.log` — eliminar un nombre.
- `unlink Home:/Documents/alias` — eliminar el enlace simbólico mismo,
  no lo que señala.
- `unlink -- -nombre-raro` — eliminar un nombre que empieza por guion.

## EXIT STATUS

- `0` — el nombre se eliminó (o se escribió la ayuda breve).
- `1` — el sistema de archivos rechazó la eliminación, o la salida
  falló; la razón se imprime en la salida de error estándar.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
