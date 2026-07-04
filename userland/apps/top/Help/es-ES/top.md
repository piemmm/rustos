## NAME

top — observar la lista de procesos en vivo

## SYNOPSIS

`top [-h | -?]`

## DESCRIPTION

Muestra una vista en vivo, a pantalla completa, de la lista de procesos
a través de la API de información del sistema, al estilo del `top`
clásico. Comienza con los procesos del llamante; el servicio concede la
vista de todo el sistema únicamente a un llamante que posea
`CAP_SYSINFO_GLOBAL`.

El visor no acepta operandos: se controla con teclas pulsadas dentro de
la sesión.

- `q` — salir.
- `a` — alternar entre sus propios procesos y la vista de todo el
  sistema.
- `r` — refrescar la lista.
- Arriba/Abajo, RePág/AvPág, Inicio/Fin — mover la selección.
- `h`, `?` — mostrar u ocultar el resumen de teclas.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de este comando y salir. Dentro de
  una sesión en curso, las mismas teclas alternan el resumen de teclas.

## EXIT STATUS

- `0` — la sesión terminó con `q`, o se mostró la ayuda corta.
- `1` — el servicio o el terminal falló.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
