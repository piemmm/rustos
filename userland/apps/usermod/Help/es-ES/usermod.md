## NAME

usermod — modificar una cuenta de usuario

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Cambia los campos de identidad de una cuenta, su estado de bloqueo o su
techo de capacidades. Modificar una cuenta es una operación administrativa:
la base rechaza a quien no posea la capacidad de administración de usuarios.

Una edición de identidad sustituye todo el conjunto de campos no
relacionados con la seguridad, así que la herramienta lee primero el
registro actual y reenvía sin cambios cada campo que nadie nombró. Una
cuenta que la base no lista se rechaza antes de enviar nada.

Cada conmutador es su propia operación de base, aplicada de una en una y
entera o nada. Una línea que pide varias emite varias, en un orden fijo —
campos, luego capacidades, luego bloqueo — y se detiene en el primer
rechazo, nombrando el paso y advirtiendo de que un cambio anterior puede
estar ya en vigor.

`-G` sustituye todo el conjunto suplementario en lugar de añadir: la base
toma conjuntos enteros, y añadir sobre una lectura obsoleta sería peor que
una sustitución explícita. `--grants` es un concepto propio de TAIRiX,
escrito sólo en forma larga; la base rechaza cualquier capacidad que la
cuenta llamante no posea.

`--` termina el análisis de opciones: todo argumento posterior es un operando.

## OPTIONS

- `-c, --comment COMMENT` — el comentario / nombre completo de la cuenta.
- `-d, --home HOME` — el directorio personal.
- `-s, --shell SHELL` — el intérprete de inicio de sesión.
- `-g, --gid GID` — el identificador numérico del grupo principal.
- `-G, --groups LIST` — los identificadores numéricos de grupos
  suplementarios, separados por comas, que sustituyen al conjunto actual.
  Una lista vacía lo borra.
- `-L, --lock` — impedir el inicio de sesión de la cuenta.
- `-U, --unlock` — permitirlo de nuevo.
- `--grants LIST` — los nombres de capacidades, separados por comas, que
  forman todo el techo. Una lista vacía lo borra.
- `-h, -?, --help` — mostrar la ayuda breve propia de esta orden.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — fijar el nombre completo de la cuenta.
- `usermod -L ada` — bloquear la cuenta.
- `usermod --grants LIST` — sustituir el techo de capacidades.

## EXIT STATUS

- `0` — se hicieron todos los cambios pedidos.
- `1` — la base rechazó o no pudo completar un cambio; el paso y la razón se
  imprimen en la salida de error.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
