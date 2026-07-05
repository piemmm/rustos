## NAME

reset — devolver el terminal a un estado sano

## SYNOPSIS

`reset`

## DESCRIPTION

Deshace el estado que un programa a pantalla completa colgado puede
dejar atrás. Primero la disciplina de entrada vuelve al valor
interactivo por defecto (los caracteres tecleados vuelven a verse).
Después se escribe la secuencia de restauración: salir de la pantalla
alternativa, mostrar el cursor, reiniciar colores y atributos,
reiniciar la región de desplazamiento y, por último, llevar el cursor a
la esquina superior izquierda y borrar la pantalla.

Las operaciones emitidas dependen del terminal nombrado en `TERM`; una
operación que el terminal no entiende se omite. Un terminal sin ningún
control (un `TERM` desconocido degrada al perfil mínimo) recibe solo la
restauración de la disciplina de entrada.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `reset` — restaurar el terminal tras el fallo de un programa a
  pantalla completa.

## EXIT STATUS

- `0` — el terminal fue restaurado.
- `1` — la salida no pudo entregarse.
- `2` — la línea de órdenes no fue comprendida.

## ENVIRONMENT

- `TERM` — el terminal cuya secuencia de restauración se escribe.
- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `clear`
- `man`
