## NAME

seq — imprimir una secuencia de números

## SYNOPSIS

`seq [-f format] [-s string] [-w] [primero [incremento]] último`

## DESCRIPTION

Imprime los números desde `primero` hasta `último`, en pasos de
`incremento`, uno por línea de forma predeterminada. Un `primero` o un
`incremento` omitido vale 1 — incluso cuando `último` es menor que
`primero`, de modo que `seq 5 1` no imprime nada. La secuencia termina
cuando sumar `incremento` sobrepasaría `último`.

Los tres operandos se leen como valores de coma flotante; `incremento`
suele ser positivo cuando `primero` es menor que `último` y negativo en
caso contrario, y no puede ser cero. `último` puede ser `inf` para
contar sin fin. La precisión de salida predeterminada sigue la
escritura de los operandos (`seq 1 0.25 2` imprime dos decimales), y
las secuencias de enteros se generan de forma exacta, por grandes que
sean los números.

El análisis de opciones se detiene en el primer operando, y un número
negativo inicial es un operando, no una opción: `seq -5 5` cuenta
desde -5.

## OPTIONS

- `-f, --format <format>` — imprimir cada número mediante el `<format>`
  de coma flotante al estilo printf (una única directiva `%` de tipo
  `e`, `f`, `g` o `a`, en mayúscula o minúscula, con los indicadores,
  anchura y precisión habituales). No puede combinarse con `-w`.
- `-s, --separator <string>` — separar los números con `<string>` en
  lugar de un salto de línea. La salida sigue terminando con un salto
  de línea.
- `-w, --equal-width` — rellenar cada número con ceros iniciales hasta
  una anchura común. No puede combinarse con `-f`.
- `-h, -?` — mostrar la ayuda breve de esta orden.
- `--` — terminar el análisis de opciones; todo argumento posterior es
  un operando.

## EXAMPLES

- `seq 5` — imprimir del 1 al 5.
- `seq 2 5` — imprimir del 2 al 5.
- `seq 1 2 10` — imprimir los impares del 1 al 9.
- `seq 5 -1 1` — contar hacia atrás de 5 a 1.
- `seq -w 8 10` — imprimir `08`, `09`, `10`.
- `seq -s , 3` — imprimir `1,2,3`.
- `seq -f %.2f 3` — imprimir `1.00`, `2.00`, `3.00`.

## EXIT STATUS

- `0` — se escribió la secuencia (o la ayuda breve solicitada).
- `1` — la salida dejó de aceptar bytes.
- `2` — la línea de órdenes no se entendió (opción desconocida, número
  inválido, incremento cero o formato incorrecto).

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `yes`
- `man`
