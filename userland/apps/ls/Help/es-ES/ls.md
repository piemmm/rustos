## NAME

ls — listar el contenido de los directorios

## SYNOPSIS

`ls [-aAdFghlmnopQrRS1] [--] [path...]`

## DESCRIPTION

Lista cada operando de ruta: las entradas de un operando de directorio
se leen y se listan (salvo que `-d` designe el directorio en sí),
cualquier otro operando se lista tal cual. Sin operandos se lista el
directorio actual (`.`).

Las entradas se ordenan por nombre (o por tamaño, la mayor primero,
con `-S`; invertidas con `-r`), un nombre por línea de forma
predeterminada. Las entradas cuyo nombre empieza por `.` se ocultan
salvo que se dé `-a` o `-A`; cuando se ocultan entradas, se emite una
nota en el flujo de información estándar (fd 3), nunca en la lista
misma.

El formato largo (`-l`) muestra los bits de tipo y permisos, el
propietario y el grupo, el tamaño y luego el nombre. El propietario y
el grupo son identificadores numéricos: resolver nombres de cuentas
exige la base de datos de usuarios protegida por capacidad, que un
listado no debe exigir; la salida coincide por tanto con el repliegue
numérico de la herramienta GNU (`-n` produce lo mismo). No hay columna
de número de enlaces ni de marca de tiempo porque el contrato del
sistema de archivos aún no lleva enlaces duros ni marcas de tiempo;
las columnas aparecerán cuando lo haga.

Cuando se dan varios operandos — y siempre bajo `-R` — la lista de
cada directorio va precedida de un encabezado `ruta:`, y los bloques
se separan con una línea en blanco.

## OPTIONS

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
- `-m` — nombres separados por comas en una línea.
- `-n, --numeric-uid-gid` — formato largo con propietario y grupo
  numéricos; implica `-l`. El propietario y el grupo siempre son
  numéricos aquí (véase arriba), así que coincide con `-l`.
- `-o` — formato largo sin la columna de grupo; implica `-l`.
- `-p` — añadir `/` a los directorios.
- `-Q, --quote-name` — entrecomillar cada nombre, escapando comillas,
  barras invertidas y caracteres de control.
- `-r, --reverse` — invertir el orden de clasificación.
- `-R, --recursive` — listar los subdirectorios recursivamente.
- `-S` — ordenar por tamaño, el mayor primero.
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
