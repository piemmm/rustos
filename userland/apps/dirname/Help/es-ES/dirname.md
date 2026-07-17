## NAME

dirname — quitar el último componente de los nombres

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Imprime cada ruta escrita sin su último componente: se quitan las
barras finales, luego el último componente y las barras que lo
preceden. La operación es puramente léxica: ninguna ruta se resuelve ni
se toca en el disco. Una ruta sin barra restante tiene como padre `.`;
un padre que queda vacío es la raíz.

Una raíz nunca se recorta: `dirname /tools` es `/`, y — el equivalente
en el bosque de almacenamiento de TAIRiX — `dirname Home:/tools` es
`Home:/`. Una raíz de alias (`Home:/`, `System:/`, …) desempeña
exactamente el papel que `/` desempeña en los sistemas POSIX.

## OPTIONS

- `-z, --zero` — terminar cada resultado con NUL en lugar de salto de
  línea.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `dirname /System/Apps/top.app` — imprimir `/System/Apps`.
- `dirname src/lib.rs` — imprimir `src`.
- `dirname file` — imprimir `.` (sin parte de directorio).
- `dirname Home:/tools` — imprimir `Home:/` (una raíz nunca se
  recorta).

## EXIT STATUS

- `0` — se escribieron los resultados (o la ayuda breve).
- `1` — la salida no pudo entregarse.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `basename`
- `man`
