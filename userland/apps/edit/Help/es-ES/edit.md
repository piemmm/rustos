## NAME

edit — editor de texto a pantalla completa

## SYNOPSIS

`edit [archivo] [-h | -?]`

## DESCRIPTION

Un editor de texto a pantalla completa en el espíritu del clásico
editor de QuickBasic / MS-DOS: una barra de menús arriba, el texto
debajo y una línea de estado con el nombre del archivo, la posición
del cursor y las teclas principales. Edita un archivo a la vez.

Iniciado con un operando `archivo`, el editor carga ese archivo; un
archivo que aún no existe se abre como búfer vacío y se crea al
primer guardado. Iniciado sin operando, abre un búfer sin nombre y
pide un nombre al guardarlo por primera vez.

El menú (se abre con `F10`, se recorre con las flechas, `Enter`
selecciona, `F10` cierra) ofrece:

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

Cuando una acción descartaría cambios sin guardar (`New`, `Open...`,
`Exit`), el editor pregunta primero: `y` guarda y continúa, `n`
descarta, `c` (o `F10`) cancela.

Teclas dentro de la sesión:

- Escribir inserta en el cursor; `Insert` alterna el modo de
  sobrescritura (`OVR` en la línea de estado).
- `Enter` divide la línea; `Backspace` y `Delete` borran caracteres
  y unen líneas en los finales de línea.
- Las flechas, `Home`, `End`, `PageUp`, `PageDown` mueven el cursor;
  la vista se desplaza, también en horizontal, para seguirlo.
- `Tab` inserta espacios hasta la siguiente parada de ocho columnas.
- `F1` muestra el resumen de teclas, `F2` guarda, `F3` repite la
  última búsqueda, `F10` abre el menú.

`Find...` busca hacia delante desde el cursor, literalmente y
distinguiendo mayúsculas, dando la vuelta al final del búfer; una
búsqueda sin resultado informa `Match not found` y deja el cursor
donde estaba.

El editor solo edita archivos de texto, y dice exactamente lo que
cambia:

- El archivo debe ser texto UTF-8 de como máximo 16 MiB; cualquier
  otra cosa (un archivo binario, un retorno de carro suelto, un
  archivo demasiado grande) se rechaza indicando el motivo — nunca
  se abre como basura.
- Las tabulaciones se expanden a espacios en paradas de ocho columnas
  al cargar, y los finales de línea CRLF pasan a LF; cada conversión
  se anuncia en la línea de estado, nunca se aplica en silencio.
- La presencia o ausencia del salto de línea final del archivo se
  conserva.

Una carga o un guardado rechazados dentro de la sesión se informan en
la línea de estado y el búfer se conserva; la sesión nunca muere por
un archivo rechazado. Cada ruta la resuelve y comprueba el núcleo
bajo la identidad del llamante — el editor no posee ninguna
autoridad especial.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de esta orden y salir.

## EXIT STATUS

- `0` — la sesión terminó mediante `File > Exit`, o se mostró la
  ayuda corta.
- `1` — el archivo nombrado no pudo cargarse (no es texto, es
  demasiado grande o fue rechazado), o el terminal falló; el motivo
  se imprime en la salida de error.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).
- `TERM` — el terminal para el que dibuja la sesión; un valor
  desconocido o ausente degrada a una base segura.

## SEE ALSO

- `cat`
- `man`
