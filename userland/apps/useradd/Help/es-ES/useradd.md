## NAME

useradd — crear una cuenta de usuario

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Añade una única cuenta a la base de datos de usuarios. El nombre de
inicio de sesión debe coincidir con `[a-z_][a-z0-9_-]*`; el grupo
primario (`-g`) es obligatorio y cada referencia a un grupo o usuario es
un identificador decimal. Crear una cuenta es una operación
administrativa: la base de datos rechaza a un llamante sin la capacidad
de administración de usuarios.

La cuenta creada no tiene **ninguna contraseña utilizable**: ninguna
contraseña coincide con ella hasta que un administrador establezca una (y
ninguna puede adivinarse), exactamente como la herramienta GNU crea una
cuenta deshabilitada. Establezca después una contraseña con el comando
`passwd` de la herramienta `users`.

Cuando se omite `-u`, el identificador se asigna automáticamente, uno
por encima del más alto existente. Cuando se omite `-d`, el directorio
personal sigue la disposición estándar `/Users/NAME`. La cuenta inicia
el intérprete predeterminado del sistema y el techo de capacidades de
sesión ordinario; un administrador lo amplía después con el comando
`grant` de la herramienta `users`.

`--` termina el análisis de opciones: cada argumento posterior es un
operando.

## OPTIONS

- `-u, --uid UID` — identificador numérico de usuario; asignado
  automáticamente cuando se omite (uno por encima del más alto
  existente).
- `-g, --gid GID` — identificador numérico del grupo primario.
  Obligatorio: no hay una política de grupo predeterminado que adivinar.
- `-G, --groups LIST` — identificadores numéricos de los grupos
  suplementarios, separados por comas.
- `-c, --comment TEXT` — comentario de la cuenta / nombre completo
  mostrado.
- `-d, --home PATH` — directorio personal; `/Users/NAME` cuando se
  omite.
- `-h, -?, --help` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `useradd -g 100 alice` — crear `alice` en el grupo primario `100` con
  un identificador asignado automáticamente.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — todos los
  campos indicados.

## EXIT STATUS

- `0` — la cuenta fue creada.
- `1` — la base de datos rechazó la creación o esta falló (por ejemplo
  una capacidad ausente, un identificador duplicado o un grupo
  desconocido); el motivo se imprime en el error estándar.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `groupadd`
- `users`
