## NAME

clear — borrar la pantalla del terminal

## SYNOPSIS

`clear [-x]`

## DESCRIPTION

Escribe la secuencia que sitúa el cursor en la esquina superior
izquierda y borra toda la pantalla, dejándola vacía. La secuencia
emitida depende del terminal nombrado en `TERM`; un terminal que no
puede borrar (un `TERM` desconocido degrada al perfil mínimo) hace que
la orden falle en lugar de imprimir bytes que el terminal mostraría
como caracteres extraños.

Las consolas de TAIRiX no conservan historial de desplazamiento, así
que no hay nada que borrar en ese sentido: `-x` (la opción GNU que
preserva el historial) se acepta por compatibilidad con scripts y no
cambia nada.

## OPTIONS

- `-x` — aceptada por compatibilidad con GNU; una consola TAIRiX no
  conserva historial, la salida es idéntica con o sin ella.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `clear` — borrar la pantalla.

## EXIT STATUS

- `0` — la secuencia de borrado fue escrita.
- `1` — el terminal no puede borrar, o la salida no pudo entregarse.
- `2` — la línea de órdenes no fue comprendida.

## ENVIRONMENT

- `TERM` — el terminal cuya secuencia de borrado se escribe.
- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `reset`
- `man`
