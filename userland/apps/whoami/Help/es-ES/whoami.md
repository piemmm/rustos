## NAME

whoami — imprimir el nombre de cuenta del usuario actual

## SYNOPSIS

`whoami`

## DESCRIPTION

Imprime el nombre de usuario asociado a la identidad de este proceso,
seguido de un salto de línea, y nada más.

TAIRiX no tiene `/etc/passwd`: el identificador de usuario proviene
del registro que el núcleo mantiene del proceso llamante, y el nombre
de cuenta correspondiente proviene del directorio público de cuentas
de la API de información del sistema. Si el directorio no contiene
ningún nombre para el identificador, la orden informa
`cannot find name for user ID <uid>` y falla.

La orden no acepta operandos; un argumento es un error
`extra operand`.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de esta orden.
- `--` — terminar el análisis de opciones; cualquier argumento
  posterior sigue siendo un operando de más (`whoami` no acepta
  ninguno).

## EXAMPLES

- `whoami` — imprimir el nombre de la cuenta que ejecuta la orden.

## EXIT STATUS

- `0` — se escribió el nombre (o la ayuda corta solicitada).
- `1` — falló la lectura de la identidad, la consulta del directorio o
  la salida, o el directorio no contiene ningún nombre para el
  identificador de usuario.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `users`
- `ps`
