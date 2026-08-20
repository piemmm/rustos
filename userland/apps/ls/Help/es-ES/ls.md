## NAME

ls — listar el contenido de los directorios

## SYNOPSIS

`ls [-aABbCcdFfGghikIlmNnopQqrRsSTtUuvXx1] [-w cols] [-I PATTERN]`
`[--block-size=SIZE] [--si] [--format=WORD] [--indicator-style=WORD]`
`[--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD]`
`[--quoting-style=STYLE] [--full-time] [--author] [--file-type]`
`[--group-directories-first] [--zero] [--color[=WHEN]] [--] [path...]`

## DESCRIPTION

Lista cada operando de ruta: las entradas de un operando de directorio
se leen y se listan (salvo que `-d` designe el directorio en sí),
cualquier otro operando se lista tal cual. Sin operandos se lista el
directorio actual (`.`).

Las entradas se ordenan por nombre (o por tamaño, la mayor primero,
con `-S`; por marca de tiempo, la más reciente primero, con `-t`;
invertidas con `-r`), un nombre por línea de forma
predeterminada. Las entradas cuyo nombre empieza por `.` se ocultan
salvo que se dé `-a` o `-A`; cuando se ocultan entradas, se emite una
nota en el flujo de información estándar (fd 3), nunca en la lista
misma.

El formato largo (`-l`) muestra los bits de tipo y permisos, el
propietario y el grupo, el tamaño y luego el nombre. El propietario y
el grupo son identificadores numéricos: resolver nombres de cuentas
exige la base de datos de usuarios protegida por capacidad, que un
listado no debe exigir; la salida coincide por tanto con el repliegue
numérico de la herramienta GNU (`-n` produce lo mismo). La columna de
marca de tiempo muestra la hora de modificación de forma
predeterminada; `-c`, `-u` y `--time` eligen cuál de las cuatro marcas
se muestra (y por cuál se ordena), y `--time-style` — o `--full-time`
— fija su formato. Aún no hay columna de número de enlaces porque el
contrato del sistema de archivos todavía no lleva enlaces duros;
aparecerá cuando lo haga.

Cuando se dan varios operandos — y siempre bajo `-R` — la lista de
cada directorio va precedida de un encabezado `ruta:`, y los bloques
se separan con una línea en blanco.

Un enlace simbólico se muestra con la letra de tipo `l` y, en el formato
largo, como `nombre -> destino` — el destino exactamente como está
guardado, sin resolver, que es lo que el enlace contiene. Un enlace
colgado se lista por tanto con normalidad; solo una postura que lo
resuelva (`-L`, o `-H` para un operando) informa de un destino
inalcanzable.

## OPTIONS

- `-t` — ordenar por la marca de tiempo mostrada, la más reciente
  primero.
- `-c` — usar la hora de cambio de metadatos (ctime): con `-l`
  mostrarla y con `-t` ordenar por ella; sin `-l`, ordenar por ella.
- `-u` — como `-c`, pero la hora de acceso (atime).
- `-i, --inode` — mostrar el número de nodo de cada entrada.
- `-B, --ignore-backups` — no listar las entradas cuyo nombre termina
  en `~`, en todos los modos (las copias se ocultan incluso con `-a`).
- `-I, --ignore=PATTERN` — no listar las entradas que coincidan con el
  patrón glob `PATTERN` (repetible); se aplica en todos los modos.
- `--hide=PATTERN` — como `--ignore`, pero sin efecto cuando se indica
  `-a` o `-A`.
- `--time=WORD` — qué marca mostrar y por cuál ordenar: `atime`
  (`access`, `use`), `ctime` (`status`), `mtime` (`modification`) o
  `birth` (`creation`).
- `--time-style=STYLE` — formato de la marca: `locale` (predeterminado),
  `long-iso`, `full-iso` o `iso`. Un `+FORMAT` propio no se admite.
- `--full-time` — como `-l --time-style=full-iso`.
- `-a, --all` — no ocultar las entradas cuyo nombre empieza por `.`.
- `-A, --almost-all` — como `-a`, pero sin listar nunca `.` ni `..`.
- `-d, --directory` — listar los operandos de directorio en sí, no su
  contenido.
- `-F, --classify` — añadir `/` a los directorios y `*` a los
  ejecutables.
- `-g` — formato largo sin la columna de propietario; implica `-l`.
- `-h, --human-readable` — con `-l`, mostrar tamaños como `1.1K`,
  `23M` (potencias de 1024).
- `-l` — formato largo: bits de permisos, propietario, grupo, tamaño
  y luego nombre.
- `-m` — nombres separados por comas, ajustados al ancho.
- `-n, --numeric-uid-gid` — formato largo con propietario y grupo
  numéricos; implica `-l`. El propietario y el grupo siempre son
  numéricos aquí (véase arriba), así que coincide con `-l`.
- `-o` — formato largo sin la columna de grupo; implica `-l`.
- `-p` — añadir `/` a los directorios.
- `-N, --literal` — imprimir los nombres tal cual, sin entrecomillar
  (`--quoting-style=literal`).
- `-Q, --quote-name` — entrecomillado estilo C: entrecomillar cada
  nombre, escapando comillas, barras invertidas y caracteres de
  control (`--quoting-style=c`).
- `-b, --escape` — como `-Q` pero sin las comillas circundantes y con
  los espacios escapados (`--quoting-style=escape`).
- `--quoting-style=WORD` — cómo se entrecomillan los nombres: `literal`
  (`-N`), `shell`, `shell-always`, `shell-escape`,
  `shell-escape-always`, `c` (`-Q`) o `escape` (`-b`). El valor
  predeterminado es `shell-escape` en una terminal y `literal` en caso
  contrario; los estilos `locale` y `clocale` no se admiten.
- `-q, --hide-control-chars` — mostrar los caracteres no gráficos como
  `?` (el valor predeterminado en una terminal); solo afecta a los
  estilos que no escapan.
- `--show-control-chars` — imprimir los caracteres no gráficos tal cual
  (el valor predeterminado cuando la salida no es una terminal).
- `-r, --reverse` — invertir el orden de clasificación.
- `-R, --recursive` — listar los subdirectorios recursivamente.
- `-L, --dereference` — mostrar la información del archivo que nombra cada
  enlace simbólico, en lugar del enlace, dondequiera que aparezca uno. Un
  enlace cuyo destino no se puede alcanzar se informa en la salida de error
  y el listado continúa, con estado de salida distinto de cero.
- `-H, --dereference-command-line` — desreferenciar solo los enlaces
  simbólicos nombrados en la línea de órdenes; los enlaces dentro de un
  listado se muestran como enlaces. Gana el último de `-L` y `-H`.
- `--dereference-command-line-symlink-to-dir` — el comportamiento por
  omisión cuando ninguna opción de formato impone otro: un enlace de la
  línea de órdenes *a un directorio* se desreferencia, así que
  `ls linkdir` lista el directorio, mientras cualquier otro enlace se
  muestra como enlace. `-l`, `-d` y `-F` muestran en cambio cada enlace.
- `-s, --size` — mostrar el tamaño asignado de cada entrada en bloques
  de 1024 bytes (escalado con `-h`), con una línea `total` por
  directorio listado.
- `-C` — listar en columnas, rellenadas de arriba abajo
  (predeterminado en un terminal).
- `-S` — ordenar por tamaño, el mayor primero.
- `-U` — no ordenar; listar las entradas en el orden del directorio.
- `-X` — ordenar por extensión del nombre (el texto desde el último
  `.`), empates por nombre.
- `-v` — orden «versión» natural, de modo que `f2` precede a `f10`;
  empates por nombre.
- `-f` — no ordenar y mostrar todas las entradas: activa `-a` y `-U`
  y desactiva `-l` y `-s`. Se aplica en su posición, así que un
  `-l`/`-s`/indicador de orden posterior lo anula.
- `--sort=WORD` — elegir la clave de orden por nombre: `none` (`-U`),
  `size` (`-S`), `time` (`-t`), `version` (`-v`), `extension` (`-X`)
  o `name`.
- `--group-directories-first` — listar los directorios antes que las
  demás entradas; los directorios primero incluso con `-r`.
- `-w, --width <cols>` — fijar el ancho de salida en columnas;
  `0` significa ilimitado.
- `-x` — listar en columnas, rellenadas de izquierda a derecha.
- `-1` — un nombre por línea (el comportamiento predeterminado).
- `-?` — mostrar la ayuda corta de este comando (`--help` es la
  forma larga).

- `--file-type` — añadir `/` a los directorios, pero nunca `*` a los
  ejecutables (`--indicator-style=file-type`).
- `--indicator-style=WORD` — elegir el sufijo indicador por nombre:
  `none`, `slash` (`-p`), `file-type` (`--file-type`) o `classify`
  (`-F`).
- `-G, --no-group` — omitir la columna de grupo del formato largo; a
  diferencia de `-o`, no selecciona el formato largo por sí solo.
- `--author` — con `-l`, mostrar la columna de autor (el usuario
  propietario) después del propietario y antes del grupo.
- `--si` — como `-h` pero en potencias de 1000 (`1.1k`, `23M`).
- `-k, --kibibytes` — usar bloques de 1024 bytes para las celdas `-s`
  y la línea `total` (ya es el valor predeterminado; una opción de
  tamaño tiene prioridad).
- `--block-size=SIZE` — escalar los tamaños de archivo y los bloques
  `-s` por SIZE: un entero (bytes), o una unidad `K`/`M`/`G`/`T`/`P`/
  `E` (1024), una unidad `KiB` (1024) o una unidad `KB` (1000),
  opcionalmente con un coeficiente entero.
- `--format=WORD` — elegir la disposición por nombre: `long` (`-l`) o
  `verbose`, `single-column` (`-1`), `vertical` (`-C`), `across` u
  `horizontal` (`-x`), o `commas` (`-m`).
- `-T, --tabsize <cols>` — establecer el paso de tabulación de la
  cuadrícula de columnas (8 por defecto); `0` rellena solo con
  espacios.
- `--zero` — terminar cada línea con NUL en lugar de nueva línea;
  también selecciona una sola columna, entrecomillado literal y
  caracteres de control visibles.

- `--color[=WHEN]` — colorear los nombres por tipo (directorios,
  ejecutables, archivos simples). `WHEN` es `auto` (el valor
  predeterminado: colorear solo cuando la salida es un terminal
  atestiguado), `always` (colorear incluso cuando no lo es, p. ej. una
  consola serie) o `never`; `--color` sin `WHEN` equivale a `always`. La
  salida canalizada o redirigida nunca se colorea.

## EXAMPLES

- `ls` — listar el directorio actual.
- `ls -al /System` — listado en formato largo de `/System`, entradas
  ocultas incluidas.
- `ls -lhS` — formato largo, tamaños legibles, el mayor primero.
- `ls -R Documents` — recorrer `Documents` recursivamente, un
  encabezado por directorio.
- `ls -F` — marcar los directorios con `/` y los ejecutables con `*`.
- `ls -d Documents` — listar la entrada `Documents` en sí, no su
  contenido.

## EXIT STATUS

- `0` — todos los operandos fueron listados.
- `1` — un operando no pudo inspeccionarse o un directorio no pudo
  leerse, o la salida no pudo entregarse.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

- `TERM` — el tipo de terminal, que decide la profundidad de color de
  la salida `--color`. Un `TERM` sin definir o sin color produce texto
  simple con `auto`.

## SEE ALSO

- `cat`
- `man`
