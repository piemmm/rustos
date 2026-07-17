## NAME

lspci — listar los dispositivos PCI/PCIe descubiertos

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Muestra, una línea por función PCI/PCIe descubierta, el identificador
de nodo del árbol de hardware de la función, su clase y los nombres de
su fabricante y dispositivo. El inventario es el árbol de hardware —
el inventario único de dispositivos del sistema — leído a través de la
API de información del sistema, que exige la capacidad
`CAP_SYSINFO_HW`; un rechazo se informa en la salida de error estándar
y no se lista nada en su lugar.

Los nombres provienen de la instantánea verificada de la base pública
de identificadores PCI que este comando incluye en su propio paquete.
Una identidad que la base no nombra se muestra en su forma numérica
(`Vendor 8086`, `Device 2922`, `Class 0106`), nunca inventada, y el
número de tales dispositivos se anota en el flujo de información
estándar (fd 3). Si la tabla incluida falta o no supera la
validación, el listado degrada a identificadores numéricos con la
razón en la salida de error estándar — el inventario en sí sigue
listándose.

TAIRiX no registra una dirección PCI `bus:device.function`: la
dirección estable de una función es su identificador de nodo del árbol
de hardware, mostrado como `#<node>`, y `-s` selecciona ese
identificador (una divergencia deliberada y documentada respecto al
`lspci` de Linux). La vista `-k` (controlador del núcleo) aún no se
ofrece: el sistema no publica registros de vinculación de
controladores, y `lspci` solo informa de lo que el sistema realmente
registra.

## OPTIONS

- `-n` — solo identificadores numéricos: el código de clase y
  `vendor:device` en hexadecimal.
- `-nn` — los nombres seguidos de los identificadores numéricos entre
  corchetes.
- `-v` — tras cada función, listar los recursos que su nodo declara
  (ventanas MMIO, líneas IRQ, puertos de E/S, restricciones DMA) —
  las solicitudes de concesión registradas, no estado en vivo.
- `-t` — representar las funciones como un árbol bajo sus buses
  padres.
- `-d [<vendor>]:[<device>]` — listar solo las funciones que
  coincidan con los identificadores dados (hexadecimal); una mitad
  omitida coincide con cualquiera.
- `-s <node>` — listar solo la función con el identificador de nodo
  dado (decimal).
- `-?, --help` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `lspci` — cada función PCI descubierta, con nombres.
- `lspci -nn` — lo mismo, con los identificadores numéricos al lado.
- `lspci -v -s 7` — la línea del nodo 7 más sus recursos declarados.
- `lspci -d 1af4:` — cada función del fabricante `1af4` (virtio).
- `lspci -t` — las funciones bajo su topología de bus.

## EXIT STATUS

- `0` — se escribió el listado (o la ayuda corta).
- `1` — la consulta del árbol de hardware fue rechazada o falló, o la
  salida no pudo escribirse.
- `2` — la línea de comandos no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `sysinfo`
- `man`
