## NAME

groupadd — crear un grupo

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

Añade un único grupo al registro de grupos. El nombre del grupo debe
coincidir con `[a-z_][a-z0-9_-]*` y el identificador es un valor
decimal. Crear un grupo es una operación administrativa: el registro
rechaza a un llamante sin la capacidad de administración de usuarios.

Cuando se omite `-g`, el identificador del grupo se asigna
automáticamente, uno por encima del más alto existente. Un identificador
solicitado que ya está ocupado se rechaza; el registro es la autoridad
sobre las colisiones.

`--` termina el análisis de opciones: cada argumento posterior es un
operando.

## OPTIONS

- `-g, --gid GID` — identificador numérico del grupo; asignado
  automáticamente cuando se omite (uno por encima del más alto
  existente).
- `-h, -?, --help` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `groupadd staff` — crear `staff` con un identificador asignado
  automáticamente.
- `groupadd -g 100 staff` — crear `staff` con el identificador `100`.

## EXIT STATUS

- `0` — el grupo fue creado.
- `1` — el registro rechazó la creación o esta falló (por ejemplo una
  capacidad ausente o un identificador duplicado); el motivo se imprime
  en el error estándar.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `useradd`
- `users`
