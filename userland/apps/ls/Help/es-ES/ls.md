## NAME

ls — listar el contenido de los directorios

## SYNOPSIS

`ls [-aABbCcdFfghiIlmNnopQqrRsStUuvXx1] [-w cols] [-I PATTERN]`
`[--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD]`
`[--quoting-style=STYLE] [--full-time] [--group-directories-first]`
`[--] [path...]`

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

## SEE ALSO

- `cat`
- `man`
