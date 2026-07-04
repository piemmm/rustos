## NAME

ls — listar el contenido de los directorios

## SYNOPSIS

`ls [-a] [-l] [--] [path...]`

## DESCRIPTION

Lista cada operando de ruta en orden. Para un directorio se listan sus
entradas, ordenadas por nombre; un operando que no es un directorio se
lista por su nombre. Sin operandos se lista el directorio actual.

Las entradas cuyo nombre empieza por `.` se ocultan salvo que se
indique `-a`. Cuando el filtro predeterminado oculta entradas, `ls`
anota cuántas en el flujo consultivo (fd 3); el listado en sí no
cambia.

Con más de un operando, primero se listan los que no son directorios
(ordenados por nombre) y después cada directorio bajo una cabecera
`ruta:`, con los bloques separados por una línea en blanco.

El formato largo imprime, por entrada: un carácter de tipo (`d` para un
directorio, `-` en otro caso), los nueve bits de permisos, el tamaño en
bytes alineado a la derecha en el bloque y, por último, el nombre.

## OPTIONS

- `-a, --all` — no ocultar las entradas cuyo nombre empieza por `.`.
- `-l, --long` — formato largo: tipo y bits de permisos, tamaño y
  nombre.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `ls` — listar el directorio actual.
- `ls -la /System/Apps` — listar todas las entradas de `/System/Apps`,
  incluidas las ocultas, en formato largo.
- `ls -- -a` — listar el fichero o directorio llamado `-a`.

## EXIT STATUS

- `0` — se listó cada operando.
- `1` — no se pudo inspeccionar un operando, no se pudo leer un
  directorio o no se pudo entregar el listado.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
