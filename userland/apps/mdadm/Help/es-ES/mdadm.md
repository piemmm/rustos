## NAME

mdadm — inspeccionar y administrar matrices RAID

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Inspecciona y administra las matrices RAID por software que el compositor
de matrices ensambla a partir de los dispositivos miembros. El inventario
de matrices y dispositivos se lee a través de la API de información del
sistema — la misma interfaz, al mismo nivel `CAP_SYSINFO_HW` con el que
se lee el árbol de hardware. Las mutaciones de crear, añadir, quitar y
detener se envían al punto de control del compositor, que comprueba que
quien llama posee `CAP_STORAGE_ADMIN` antes de actuar. Una negativa se
informa en la salida de error con un código de salida distinto de cero;
nada se inventa y no se presume ninguna autoridad.

Se indica exactamente un modo por invocación.

TAIRiX no tiene `/dev`, así que los dos nombres que Linux mdadm escribe
como archivos de dispositivo se escriben aquí de otra forma — una
divergencia deliberada y documentada:

- Un dispositivo se nombra por el identificador de su nodo en el árbol de
  hardware, escrito `node:<id>`, el mismo nombre que muestran los
  informes. Cualquier otra grafía se rechaza en lugar de adivinarse.
- Una matriz se nombra por su identidad de 128 bits en hexadecimal. Se
  acepta la identidad completa de 32 dígitos, así como cualquier prefijo
  que nombre exactamente una matriz; un prefijo que coincide con más de
  una matriz se rechaza en lugar de adivinar cuál se quería.

TAIRiX compone los niveles RAID 0, 1, 5, 6, 10 y la triple paridad. No
tiene RAID4, así que `--level=4` se rechaza con esa razón.

Un contexto consultivo conciso — una matriz degradada, o dispositivos en
blanco no mostrados en la vista de matrices — se escribe en el flujo de
información estándar (fd 3). Es opcional y nunca cambia la salida
principal.

## OPTIONS

- `-C, --create` — crear una matriz sobre los dispositivos nombrados e
  imprimir la identidad que el compositor le asigna.
- `-D, --detail` — informar de la identidad, el nivel, la salud, el
  número de dispositivos, la geometría y cualquier posición de
  reconstrucción o verificación de cada matriz. Sin operando de matriz,
  informar de todas las matrices.
- `-E, --examine` — listar todos los dispositivos que retiene el
  compositor: los miembros de matrices con su ranura y estado, y los
  dispositivos en blanco no afiliados sobre los que se puede crear una
  nueva matriz.
- `-a, --add` — admitir un dispositivo en blanco en una ranura ausente de
  una matriz y reconstruirlo.
- `-r, --remove` — retirar un dispositivo miembro de una matriz.
- `-S, --stop` — detener una matriz activa y liberar sus miembros.
- `-l, --level=<level>` — el nivel a crear: `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, o
  `tp`/`raid-tp` para la triple paridad.
- `-n, --raid-devices=<count>` — el número de ranuras de miembro a crear;
  debe ser igual al número de operandos de dispositivo.
- `-c, --chunk=<blocks>` — la unidad de banda en bloques lógicos; válida
  solo para un nivel con bandas.
- `-h, -?, --help` — mostrar la ayuda propia de esta orden.
- `-V, --version` — imprimir la versión y salir.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — crear una matriz RAID5 sobre tres dispositivos.
- `mdadm --detail` — informar de todas las matrices.
- `mdadm --examine` — listar todos los dispositivos, miembros y en blanco.
- `mdadm --add 3f2a node:14` — añadir un dispositivo a la matriz cuya identidad empieza por `3f2a`.
- `mdadm --stop 3f2a` — detener esa matriz.

## EXIT STATUS

- `0` — la solicitud tuvo éxito (o se escribió la ayuda).
- `1` — se denegó una capacidad, un nombre no se resolvió, el compositor
  rechazó la solicitud, o no se pudo escribir la salida.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para esta ayuda (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
