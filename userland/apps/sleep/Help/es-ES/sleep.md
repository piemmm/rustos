## NAME

sleep — pausar durante la suma de intervalos de tiempo

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Pausa durante la suma de los intervalos indicados y luego termina.

Cada `NUMBER` es un valor de coma flotante; un `SUFFIX` de una sola letra
lo escala: `s` para segundos (el valor por defecto), `m` para minutos, `h`
para horas y `d` para días. Varios operandos se suman, de modo que
`sleep 1m 30s` pausa noventa segundos. `inf` (o `infinity`) pausa hasta que
el proceso sea terminado.

A diferencia de la propia temporización de un intérprete de órdenes,
`sleep` duerme fuera del procesador: la tarea queda aparcada hasta que
transcurre el intervalo y nunca mantiene un núcleo girando en vacío.

Un valor negativo, un `nan`, un sufijo desconocido o caracteres
adicionales tras el número es un `invalid time interval`. No dar ningún
operando es un `missing operand`.

Esta orden no imprime una versión del sistema; TAIRiX no tiene tal cadena,
así que —a diferencia de GNU `sleep`— no tiene la opción `--version`.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden.
- `--` — terminar el análisis de opciones; todo argumento posterior es un
  operando.

## EXAMPLES

- `sleep 5` — pausar cinco segundos.
- `sleep 1.5h` — pausar noventa minutos.
- `sleep 1m 30s` — pausar noventa segundos (los operandos se suman).
- `sleep inf` — pausar hasta que el proceso sea terminado.

## EXIT STATUS

- `0` — el intervalo transcurrió, o se escribió una ayuda breve solicitada.
- `1` — la escritura de la ayuda breve falló.
- `2` — no se entendió la línea de órdenes (una opción desconocida, un
  operando ausente o un intervalo de tiempo no válido).

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `top`
