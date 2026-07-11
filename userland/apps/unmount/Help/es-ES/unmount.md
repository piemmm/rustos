## NAME

unmount — desmontar un volumen montado

## SYNOPSIS

`unmount [option...] name`

## DESCRIPTION

Retira del servicio el volumen montado bajo `name`: el sistema de
archivos y el dispositivo se vacían, el montaje bajo `/Storage` se
retira y la raíz duradera `id::` del volumen se revoca. `name` es el
nombre de catálogo del volumen (`usb1`) o su punto de montaje
(`/Storage/usb1`), comparado con la lista de montajes de la API de
información del sistema.

Un volumen cuyo dispositivo fue retirado con escrituras aún sin
confirmar sigue visible como `unavailable-dirty` (o
`unavailable-lost`), y un `unmount` simple lo rechaza: sus datos
retenidos se conservan para una reinserción verificada. `--force` es
la salida deliberada: los datos retenidos se descartan, el volumen se
retira y la pérdida queda registrada en el registro de auditoría. En
un volumen sano, `--force` sigue vaciando y desmontando limpiamente;
nada se descarta cuando es posible una confirmación limpia.

Desmontar requiere la autoridad de montaje (`CAP_FS_MOUNT`); el
núcleo la comprueba y audita cada decisión. Los volúmenes de arranque
permanentes y los enlaces de vista del sistema no se pueden
desmontar.

## OPTIONS

- `-f, --force` — desmontaje forzado: retirar el volumen aunque sus
  datos no puedan confirmarse, descartando los datos retenidos.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `unmount usb1` — desmontar limpiamente el volumen montado como
  `usb1`.
- `unmount /Storage/usb1` — lo mismo, nombrado por su punto de
  montaje.
- `unmount --force usb1` — retirar un volumen no disponible
  descartando sus datos retenidos.

## EXIT STATUS

- `0` — el volumen se desmontó (o se escribió la ayuda breve).
- `1` — el volumen no se encontró, no es desmontable o el núcleo
  rechazó el desmontaje.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `mount`
- `df`
- `man`
