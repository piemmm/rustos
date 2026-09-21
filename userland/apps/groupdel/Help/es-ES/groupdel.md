## NAME

groupdel — eliminar un grupo

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Retira un grupo del registro de grupos. Eliminar un grupo es una operación administrativa: el registro rechaza a quien no posea la capacidad de administración de usuarios.

El registro decide qué puede retirarse. Rechaza eliminar un grupo al que una cuenta todavía hace referencia, de modo que ninguna cuenta nombre un grupo inexistente.

`--` termina el análisis de opciones: todo argumento posterior es un operando.

## OPTIONS

- `-h, -?, --help` — mostrar la ayuda breve propia de esta orden.

## EXAMPLES

- `groupdel staff` — eliminar el grupo `staff`.

## EXIT STATUS

- `0` — el grupo fue eliminado.
- `1` — el registro rechazó o no pudo completar la eliminación (por ejemplo una capacidad ausente, un grupo desconocido o un grupo aún referenciado); la razón se imprime en la salida de error.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
