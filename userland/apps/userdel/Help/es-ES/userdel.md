## NAME

userdel — eliminar una cuenta de usuario

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Retira una cuenta de la base de datos de usuarios. Eliminar una cuenta es una operación administrativa: la base rechaza a quien no posea la capacidad de administración de usuarios.

La base decide qué puede retirarse. Rechaza eliminar la última cuenta activa que puede administrar usuarios, de modo que un sistema nunca quede sin forma de administrarse.

`--` termina el análisis de opciones: todo argumento posterior es un operando.

## OPTIONS

- `-h, -?, --help` — mostrar la ayuda breve propia de esta orden.

## EXAMPLES

- `userdel ada` — eliminar la cuenta `ada`.

## EXIT STATUS

- `0` — la cuenta fue eliminada.
- `1` — la base rechazó o no pudo completar la eliminación (por ejemplo una capacidad ausente, una cuenta desconocida o el último administrador); la razón se imprime en la salida de error.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
