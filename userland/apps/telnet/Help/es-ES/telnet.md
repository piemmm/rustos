## NAME

telnet — el cliente de terminal virtual de red (RFC 854)

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Abre una conexión TCP con un anfitrión y le transmite el terminal: la salida
del anfitrión aparece en la salida estándar, las pulsaciones van al anfitrión y
el carácter de escape (`^]` por omisión) abre el intérprete de órdenes
`telnet>`. Sin anfitrión, `telnet` arranca en ese indicador y `open` conecta.

Es tanto la forma de alcanzar un servicio orientado a líneas en otra máquina
como la forma de interrogar cualquier servicio TCP a mano: `telnet host 80`
abre una conexión en la que se puede escribir una petición.

El anfitrión puede ser un nombre o una dirección IPv4/IPv6 literal. Un nombre
se resuelve mediante el resolvedor trivial del sistema, que lee los servidores
DNS recursivos configurados a través de la API de información del sistema. El
puerto es un número: no hay base de datos de servicios, así que un *nombre* de
servicio es un error de uso y no un retorno silencioso al puerto 23.

La negociación de opciones sigue la RFC 855 con la disciplina sin bucles de la
RFC 1143, de modo que un par que se repite nunca hace que el cliente se
repita. Las opciones implementadas son BINARY, ECHO, SUPPRESS GO AHEAD,
STATUS, TIMING MARK, TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW CONTROL,
LINEMODE y NEW-ENVIRON; cualquier otra se rechaza, que es lo que significa una
opción no implementada. LINEMODE (RFC 1184) está implementado por completo —la
máscara `MODE`, la tabla de caracteres locales (SLC) y `FORWARDMASK`— de modo
que el cliente edita la línea como el servidor le pide, con los caracteres que
el servidor negocia.

El tamaño de la ventana se comunica por NAWS al conectar y cada vez que
cambia. TAIRiX no tiene señal de cambio de tamaño, así que el tamaño se vuelve
a leer cada vez que se escribe; un redimensionado llega al anfitrión en la
siguiente pulsación.

`NEW-ENVIRON` revela **solo** las variables que se definen y exportan con la
orden `environ`; el cliente nunca envía su propio entorno. `-a` y `-l`
exportan un nombre de acceso, y eso es lo único que una invocación revela por
sí misma.

Dos órdenes de la herramienta histórica faltan deliberadamente. No hay escape
al intérprete `!`: a un programa que analiza datos de red hostiles no se le da
la autoridad de lanzar un intérprete. No hay `slc check`, porque la RFC 1184
no le da forma alguna en el cable distinta de `slc export`. La interfaz de
sockets no expone datos urgentes de TCP, así que un Synch viaja como la sola
Data Mark. Cuando la entrada estándar llega al fin de fichero —una invocación
redirigida como `telnet host 80 < peticion`— se cierra solo el lado de envío y
la sesión sigue leyendo hasta que el anfitrión remoto también cierra, de modo
que la respuesta no se descarta como hace la herramienta histórica.

## OPTIONS

- `-4, --ipv4` — conectar solo por IPv4.
- `-6, --ipv6` — conectar solo por IPv6.
- `-8, --binary` — pedir una vía de datos de 8 bits en ambos sentidos.
- `-L, --eight-bit-output` — pedir una vía de 8 bits solo en la salida.
- `-E, --no-escape` — sin carácter de escape; todo va al anfitrión.
- `-e, --escape <char>` — fijar el carácter de escape (`^]`, `^A`, un solo
  carácter, o vacío para ninguno).
- `-a, --login` — exportar el nombre de acceso de la sesión por `NEW-ENVIRON`.
- `-l, --user <name>` — exportar `name` como nombre de acceso (implica `-a`).
- `-b, --bind <address>` — enlazar esta dirección local antes de conectar.
- `-d, --debug` — trazar la negociación de opciones en la salida de error.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `telnet example.test` — abrir una sesión en el puerto telnet asignado.
- `telnet 10.0.2.2 25` — hablar a mano con un servicio de correo.
- `telnet -6 fe80::2` — conectar solo por IPv6.
- `telnet -l ada host` — ofrecer `ada` como nombre de acceso.
- `telnet -8 host` — pedir una vía de 8 bits en ambos sentidos.
- `telnet` y luego `open host` — conectar desde el indicador de órdenes.

## EXIT STATUS

- `0` — la sesión se produjo (como quiera que el anfitrión la terminase), o se
  escribió la ayuda breve.
- `1` — no pudo haber sesión: el anfitrión no se resolvió, el socket fue
  rechazado, o el terminal no pudo pasar a modo directo.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `TERM` — comunicado al anfitrión mediante la opción TERMINAL TYPE.
- `USER` — el nombre de acceso que exporta `-a`.
- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
