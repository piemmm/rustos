## NAME

ss — listar los sockets abiertos

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Lista los sockets abiertos del sistema, una fila por socket: el
protocolo de transporte, el estado de la conexión, la ocupación de las
colas de recepción y de envío, la `address:port` local y remota y —con
`-p`— el proceso propietario.

Las filas provienen del listado de sockets de la API de Información del
Sistema, que la pila de red responde como una consulta privilegiada y
auditada: nombra los sockets de cada principal y el par de cada
conexión, de modo que listar todos los sockets exige
`CAP_SYSINFO_GLOBAL`. No hay `/proc/net`; a una sesión sin esa capacidad
se le indica y `ss` termina, en vez de imprimir una tabla vacía.

Por omisión el listado muestra los sockets conectados, no en escucha.
`-l` muestra solo los sockets en escucha y `-a` ambos; la cuenta de
oyentes ocultos se anota en el flujo de información estándar (fd 3),
nunca en la tabla. `-t` y `-u` restringen el protocolo y `-4`/`-6` la
familia de direcciones; sin ninguno, se muestran todos los protocolos y
familias. Los puertos son siempre numéricos (TAIRiX no tiene base de
nombres de servicio), así que `-n` se acepta pero está siempre en vigor
para ellos. Las direcciones también son numéricas salvo que `-r` pida
nombres de host: `-r` resuelve cada una con el resolvedor del sistema
(una consulta `PTR`), consulta cada dirección distinta una sola vez y
deja numérica la que no tiene nombre. Una dirección no especificada se
imprime como `*` y un puerto no ligado como `*`; una dirección IPv6 va
entre corchetes para que el separador `:port` quede sin ambigüedad — un
nombre resuelto no necesita corchetes.

`ss` solo acepta opciones. La gramática de expresiones de filtro de
iproute2 (filtros de estado y de dirección) no está implementada, así
que un operando suelto es un error de uso y no un argumento ignorado en
silencio.

## OPTIONS

- `-t, --tcp` — mostrar los sockets TCP. Sin `-t` ni `-u`, se muestran
  ambos protocolos.
- `-u, --udp` — mostrar los sockets UDP.
- `-a, --all` — mostrar los sockets en escucha y conectados.
- `-l, --listening` — mostrar solo los sockets en escucha.
- `-n, --numeric` — no resolver nombres de servicio. Siempre en vigor
  en TAIRiX; aceptado por familiaridad. Los nombres de host son asunto
  de `-r`.
- `-r, --resolve` — resolver las direcciones a nombres de host por DNS.
  Desactivado por omisión, así que el listado no consulta sin que se lo
  pidan.
- `-p, --processes` — añadir la columna del proceso propietario
  (`pid=N`).
- `-4, --ipv4` — restringir el listado a sockets IPv4.
- `-6, --ipv6` — restringir el listado a sockets IPv6.
- `-H, --no-header` — suprimir la línea de cabecera.
- `-s, --summary` — mostrar los totales de defensa de conexiones TCP
  de la pila en lugar de la tabla de sockets.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `ss` — los sockets conectados, no en escucha.
- `ss -a` — cada socket, en escucha y conectado.
- `ss -l` — solo los sockets en escucha.
- `ss -tlp` — los sockets TCP en escucha, con el proceso propietario.
- `ss -u4` — los sockets UDP sobre IPv4.
- `ss -r` — el mismo listado con las direcciones resueltas a nombres.

## EXIT STATUS

- `0` — se produjo el listado (o se escribió la ayuda breve).
- `1` — la consulta de sockets fue rechazada o falló, o no se pudo
  escribir la salida.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
