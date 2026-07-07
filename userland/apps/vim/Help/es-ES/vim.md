## NAME

vim — el editor de texto modal

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Edita archivos de texto con el conjunto de órdenes modal del conocido
editor vim. La sesión comienza en modo normal: las teclas son órdenes,
e `i` (o `a`, `o` y sus variantes) entra en el modo de inserción,
donde lo tecleado se convierte en texto. `Esc` vuelve al modo normal.
`:q` sale; `:wq` (o `ZZ`) escribe y sale.

Pueden nombrarse varios archivos; la sesión abre el primero y `:n` /
`:prev` recorren la lista de argumentos. Un archivo que aún no existe
es un `[New File]`, creado en la primera escritura.

Órdenes del modo normal (el núcleo de vim implementado):

- Movimientos: `h j k l`, las flechas, `w W b B e E`, `0 ^ $`,
  `f F t T` con repetición `;`/`,`, `gg G`, `{ }`, `%`, `H M L` y
  `Enter`. Un prefijo numérico repite un movimiento: `3w`.
- Operadores: `d` (borrar), `c` (cambiar), `y` (copiar), aplicados
  sobre cualquier movimiento u objeto de texto (`iw aw i( a( i[ i{ i"
  i' i<` y sus pares); duplicados (`dd cc yy`) actúan sobre líneas
  enteras. Abreviaturas: `x X s S D C Y r ~ J`.
- Registros: `"a`–`"z` antes de un operador o un pegado elige un
  registro con nombre; las mayúsculas añaden. `p`/`P` pega
  después/antes del cursor.
- Historial de cambios: `u` deshace cambios enteros, `Ctrl-R` los
  rehace, y `.` repite el último cambio (incluido su texto insertado).
- Búsqueda: `/pattern` hacia delante, `?pattern` hacia atrás, `n`/`N`
  repiten, `*` busca la palabra bajo el cursor. Los patrones admiten
  literales, `.`, `*`, `^`, `$`, clases `[...]` y los límites de
  palabra `\<` `\>`. Las coincidencias quedan resaltadas hasta `:noh`.
- Selección visual: `v` (caracteres) y `V` (líneas), extendida con
  cualquier movimiento u objeto de texto, y operada con `d x c s y J`.
- Desplazamiento: `Ctrl-D Ctrl-U` (media ventana), `Ctrl-F Ctrl-B` y
  RePág/AvPág (ventana entera); `Ctrl-G` muestra el resumen del
  archivo.

El núcleo ex (`:`): `:w [file]`, `:q`, `:wq`, `:x`, `:e file`,
`:enew`, `:r file`, `:n`, `:prev`, `:noh`, `:set number` /
`:set nonumber`, direcciones de línea (`:12`, `:$`, `:.+2`),
`:[range]d` y `:[range]s/pattern/replacement/[g]` (con `&` para la
coincidencia entera en el reemplazo, `%` para todas las líneas del
rango). Un `!` tras `w`, `q` o `e` fuerza pese al modo de solo lectura
o a los cambios sin escribir.

Todo lo que vim trae más allá de este núcleo queda previsto para
etapas posteriores; la lista vive en `plans/VIM.md` del árbol fuente.

## OPTIONS

- `-R` — solo lectura: el búfer se edita en memoria, pero `:w` se
  rechaza salvo que se fuerce con `:w!`.
- `+num` — empezar en la línea `num` del primer archivo.
- `+` — empezar en la última línea del primer archivo.
- `+/pattern` — empezar en la primera coincidencia de `pattern` en el
  primer archivo.
- `--` — fin de las opciones; todo argumento posterior es un nombre de
  archivo.
- `-h, -?` — mostrar la ayuda breve propia de esta orden y salir.

## EXIT STATUS

- `0` — la sesión terminó con una orden de salida, o se mostró la
  ayuda breve.
- `1` — el terminal falló; el motivo se imprime en la salida de error.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `fr-FR`).
- `TERM` — el perfil de terminal de la sesión; los valores desconocidos
  degradan a la base simple.

## SEE ALSO

- `man`
- `cat`
