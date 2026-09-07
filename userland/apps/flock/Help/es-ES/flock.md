## NAME

flock — ejecutar una orden manteniendo un bloqueo de archivo consultivo

## SYNOPSIS

`flock [options] archivo orden [argumento...]`

## DESCRIPTION

Toma un bloqueo consultivo que cubre todo el `archivo`, ejecuta la `orden`
mientras lo mantiene y termina con el estado propio de la orden. Dos
ejecuciones que nombren el mismo `archivo` nunca se solapan, que es lo que
necesita un guion para tener una sola copia de sí mismo en marcha.

El bloqueo pertenece al archivo abierto por esta ejecución, así que el
sistema lo libera cuando la ejecución termina: al acabar, al ser
interrumpida o al fallar. No se escribe nada en el `archivo` y no hay nada
que limpiar después, así que no queda un bloqueo obsoleto que la siguiente
ejecución deba esperar. El `archivo` se crea si no existe.

Los bloqueos son **consultivos**: coordinan programas que aceptan usarlos.
No conceden ni retiran ningún acceso, de modo que un programa que no toma
un bloqueo no queda impedido de leer o escribir. Quién puede leer o
escribir lo decide el propietario, los permisos y la lista de acceso del
archivo, igual que en cualquier otro.

Sin `-n` ni `-w` la ejecución espera todo lo necesario. Con `-n` renuncia
al instante; con `-w`, tras el tiempo indicado. En ambos casos renunciar
termina con el código de conflicto (`1` salvo que `-E` diga otra cosa) y la
orden **no** se ejecuta, así que un guion siempre distingue «no conseguí el
bloqueo» de «la orden falló».

Tres opciones de los `flock` de otros sistemas faltan a propósito en lugar
de aceptarse y descartarse. `-u` y `-o` actúan sobre un descriptor de
archivo que un intérprete abrió antes, forma que esta orden no ofrece; `-c`
pasa su argumento a un intérprete, lo que aquí se escribe explícitamente
`flock archivo elsh -c '...'` para que quede claro cuál se ejecuta. Pedir
cualquiera de ellas es un error de uso: se avisa al guion en vez de
ejecutarlo sin el bloqueo que pedía.

## OPTIONS

- `-s, --shared` — tomar un bloqueo compartido. Varias ejecuciones pueden mantenerlo a la vez, y todas excluyen a quien pida uno exclusivo. Es el bloqueo de los lectores.
- `-x, --exclusive` — tomar un bloqueo exclusivo, que excluye a cualquier otro poseedor. El valor por omisión y el bloqueo de los escritores.
- `-n, --nonblock` — no esperar: si el bloqueo está tomado, salir de inmediato con el código de conflicto.
- `-w, --timeout <seconds>` — esperar como máximo esos segundos enteros y luego salir con el código de conflicto.
- `-E, --conflict-code <n>` — el estado de salida cuando `-n` o `-w` renuncia. Por omisión `1`. Elija un valor que la orden nunca devuelva si un guion debe distinguirlos.
- `-v, --verbose` — informar por la salida de error si se tomó el bloqueo.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — lanzar la copia de
  seguridad, esperando si ya hay otra en marcha.
- `flock -n /Storage/db/data.lock compact` — compactar la base de datos, o
  salir con `1` de inmediato si el bloqueo está tomado.
- `flock -s -w 30 /Storage/db/data.lock report` — tomar un bloqueo de
  lector, esperando hasta treinta segundos a que acabe un escritor.
- `flock -E 99 -n lock task` — salir con `99` en vez de `1` cuando el
  bloqueo está tomado, para distinguirlo de un fallo de `task`.

## EXIT STATUS

- el estado propio de la orden — se tomó el bloqueo y la orden se ejecutó.
- el código de conflicto (`1` por omisión) — `-n` o `-w` renunció; la orden
  no se ejecutó.
- `1` — el bloqueo no pudo tomarse por una razón que esperar no arreglaría,
  o la orden no pudo ejecutarse; la razón se imprime en la salida de error.
- `2` — no se entendió la línea de órdenes; no se bloqueó ni se ejecutó
  nada.
- `126` — la orden se encontró pero no pudo ejecutarse.
- `127` — la orden no se encontró.

## ENVIRONMENT

- `PATH` — se recorre buscando la orden, tras los almacenes de programas
  del sistema y del usuario.
- `HOME` — localiza los almacenes de programas propios del usuario.
- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
