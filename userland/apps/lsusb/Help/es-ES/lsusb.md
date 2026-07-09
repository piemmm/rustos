## NAME

lsusb — listar los dispositivos USB detectados

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Muestra, una línea por interfaz USB detectada, los números de bus y de
dispositivo de la interfaz, su identificador `vendor:product` y los
nombres de su fabricante y de su producto. El inventario es el árbol de
hardware — el único inventario de dispositivos del sistema — leído a
través de la API de información del sistema, que exige la capacidad
`CAP_SYSINFO_HW`; un rechazo se informa en la salida de error estándar
y no se lista nada en su lugar.

Los nombres provienen de la instantánea verificada de la base pública
de identificadores USB que este comando incluye en su propio paquete.
Una identidad que la base no nombra muestra solo su forma numérica
`ID vvvv:pppp`, nunca inventada, y el número de tales dispositivos se
anota en el flujo de información estándar (fd 3). Si la tabla incluida
falta o no supera la validación, el listado se degrada a
identificadores desnudos con la razón en la salida de error estándar —
el inventario en sí sigue listándose.

RustOS no tiene el registro de números de bus/dispositivo de Linux: el
número de bus de un dispositivo es el identificador de nodo estable de
su controlador en el árbol de hardware y su número de dispositivo es su
propio identificador de nodo, y `-s` selecciona esos identificadores
(una divergencia deliberada y documentada respecto al `lsusb` de
Linux). El inventario registra un nodo por *interfaz*: un dispositivo
con varias interfaces aparece una vez por interfaz.

## OPTIONS

- `-v` — tras cada dispositivo, listar su clase, subclase y protocolo
  de interfaz (`bInterfaceClass`, `bInterfaceSubClass`,
  `bInterfaceProtocol`) con los nombres de las tablas de clases USB.
- `-t` — mostrar los dispositivos como un árbol bajo sus controladores
  y buses.
- `-d [<vendor>]:[<product>]` — listar solo los dispositivos que
  coincidan con los identificadores de fabricante/producto dados
  (hex); una mitad omitida coincide con cualquiera.
- `-s [[<bus>]:][<devnum>]` — listar solo los dispositivos que
  coincidan con los identificadores de nodo del controlador (bus) y/o
  del dispositivo (decimal); un valor sin dos puntos es un número de
  dispositivo solo.
- `-?, --help` — mostrar la ayuda breve de este comando.

## EXAMPLES

- `lsusb` — cada dispositivo USB detectado, con nombres.
- `lsusb -v` — lo mismo, con la identidad de clase de cada interfaz.
- `lsusb -s 2:` — cada dispositivo bajo el nodo controlador 2.
- `lsusb -d 046d:` — cada dispositivo del fabricante `046d`
  (Logitech).
- `lsusb -t` — los dispositivos bajo su topología de bus.

## EXIT STATUS

- `0` — el listado (o la ayuda breve) se escribió.
- `1` — la consulta del árbol de hardware fue rechazada o falló, o la
  salida no pudo escribirse.
- `2` — la línea de comandos no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
