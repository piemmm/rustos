## NAME

printf — dar formato a datos e imprimirlos

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Imprime los `argument`(s) bajo el control de `format`, como la función C
`printf`. El formato contiene tres clases de elementos: caracteres
ordinarios, copiados a la salida estándar; secuencias de escape con
barra invertida; y directivas de conversión `%`, cada una de las cuales
convierte el argumento siguiente.

Los escapes son `\a` (alerta), `\b` (retroceso), `\c` (terminar toda la
salida inmediatamente), `\e` (escape), `\f` (salto de página), `\n`
(salto de línea), `\r` (retorno de carro), `\t` (tabulación), `\v`
(tabulación vertical), `\\`, `\"`, `\NNN` (de uno a tres dígitos
octales), `\xHH` (uno o dos dígitos hexadecimales) y `\uHHHH` /
`\UHHHHHHHH` (puntos de código Unicode, cuatro u ocho dígitos
hexadecimales).

Las conversiones son `%d`/`%i` (decimal con signo), `%u` (decimal sin
signo), `%o`/`%x`/`%X` (octal y hexadecimal), `%e`/`%E`/`%f`/`%F`/
`%g`/`%G`/`%a`/`%A` (coma flotante), `%c` (el primer carácter del
argumento), `%s` (cadena), `%b` (cadena cuyos propios escapes se
interpretan, el octal se escribe `\0NNN`), `%q` (cadena entrecomillada
para reutilizarse como entrada de shell) y `%%` (un `%` literal). Una
directiva acepta los indicadores C `-`, `+`, espacio, `#`, `0` y `'`,
una anchura de campo y una precisión; la anchura y la precisión pueden
ser `*`, leyendo su valor del argumento siguiente. `%b` y `%q` no
aceptan indicadores, anchura ni precisión.

El formato se reutiliza cuanto haga falta hasta consumir todos los
argumentos; una conversión sin argumento restante imprime cero o la
cadena vacía. Un argumento numérico se lee como un número C
(hexadecimal `0x`, octal con `0` inicial, coma flotante, `inf`, `nan`);
un `'` o `"` inicial convierte el punto de código del carácter
siguiente. Un argumento que no es un número, lo es solo parcialmente o
queda fuera de rango se diagnostica en la salida de error y se
convierte hasta donde llega — la ejecución continúa y termina con
estado `1`. Una conversión desconocida, un indicador en una conversión
que no lo acepta o un escape mal formado termina la ejecución con un
diagnóstico.

Dos divergencias deliberadas respecto al `printf` de GNU: la coma
flotante se calcula en doble precisión IEEE 754 (GNU usa `long
double`), de modo que un valor más allá del rango del doble imprime
`inf`; y un *primer* argumento `-h` o `-?` muestra esta ayuda corta —
tal formato se escribe `printf -- -h...`.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de esta orden (solo como primer
  argumento).
- `--` — terminar el análisis de opciones; el argumento siguiente es el
  formato.

## EXAMPLES

- `printf '%s\n' hello` — imprimir `hello` y un salto de línea.
- `printf '%d\n' 0x10` — imprimir `16`.
- `printf '%5.2f|\n' 3.14159` — imprimir ` 3.14|`.
- `printf '%s=%q\n' greeting 'hi there'` — imprimir
  `greeting='hi there'`.
- `printf '%b' 'one\ntwo\n'` — imprimir dos líneas desde un solo
  argumento.
- `printf '%s-' a b c` — reutilizar el formato: `a-b-c-`.

## EXIT STATUS

- `0` — todo (o la ayuda corta solicitada) se escribió.
- `1` — se diagnosticó un problema de conversión, faltaba el formato o
  era inválido, un escape estaba mal formado, o la salida dejó de
  aceptar bytes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `seq`
- `man`
