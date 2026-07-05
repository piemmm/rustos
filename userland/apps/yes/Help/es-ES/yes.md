## NAME

yes — escribir repetidamente una línea de texto

## SYNOPSIS

`yes [cadena...]`

## DESCRIPTION

Escribe sus operandos, unidos por espacios simples — o `y` cuando no se
da ninguno —, seguidos de un salto de línea, una y otra vez, hasta que
su salida deja de aceptar bytes (una tubería cerrada) o el proceso
termina. Su función histórica es dar una respuesta afirmativa a una
orden que pregunta; la moderna, ser una fuente barata de texto repetido.

El análisis de opciones se detiene en el primer operando: `yes a -x`
escribe `a -x`. Una opción desconocida antes de los operandos es un
error; escriba `yes -- -x` para imprimir una cadena con aspecto de
opción.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden.
- `--` — terminar el análisis de opciones; todo argumento posterior es
  un operando.

## EXAMPLES

- `yes` — escribir `y` hasta la interrupción.
- `yes hello world` — escribir `hello world` hasta la interrupción.
- `yes -- -x` — escribir `-x` (tras `--`, los operandos pueden parecer
  opciones).

## EXIT STATUS

- `0` — se sirvió la ayuda breve solicitada.
- `1` — la salida dejó de aceptar bytes (la única condición de parada
  de la herramienta).
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `true`
- `man`
