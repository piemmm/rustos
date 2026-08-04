## NAME

basename — quitar el directorio y el sufijo de los nombres

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Imprime el componente final de cada ruta escrita: se quitan las barras
finales y después todo lo anterior a la última barra restante, esta
incluida. La operación es puramente léxica: ninguna ruta se resuelve ni
se toca en el disco. Con un `suffix` (el segundo operando, o `-s`),
también se quita un `suffix` final, salvo que sea todo el nombre
restante.

Una raíz nunca se recorta: `basename /` es `/`, y — el equivalente en
el bosque de almacenamiento de TAIRiX — `basename Home:/` es `Home:/`.
Una raíz de alias (`Home:/`, `System:/`, …) desempeña exactamente el
papel que `/` desempeña en los sistemas POSIX.

Sin `-a` ni `-s` se aceptan como máximo dos operandos: el nombre y un
sufijo opcional. Con `-a` (o `-s`, que lo implica), cada operando es un
nombre.

## OPTIONS

- `-a, --multiple` — tratar cada operando como un nombre.
- `-s, --suffix <suffix>` — quitar un `suffix` final de cada nombre;
  implica `-a`. También se escribe `--suffix=<suffix>` o agrupado
  (`-s.rs`).
- `-z, --zero` — terminar cada resultado con NUL en lugar de salto de
  línea.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `basename /System/Commands/top.app` — imprimir `top.app`.
- `basename src/lib.rs .rs` — imprimir `lib`.
- `basename -s .rs -a a.rs b.rs` — imprimir `a` y `b`.
- `basename Home:/` — imprimir `Home:/` (una raíz nunca se recorta).

## EXIT STATUS

- `0` — se escribieron los resultados (o la ayuda breve).
- `1` — la salida no pudo entregarse.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `dirname`
- `man`
