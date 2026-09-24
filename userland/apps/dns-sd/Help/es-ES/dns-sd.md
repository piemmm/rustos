## NAME

dns-sd — explorar, resolver y buscar servicios del enlace local

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Pregunta a la red local — el enlace — por los servicios que ofrece, a través
del servicio de descubrimiento del enlace local del sistema. `dns-sd` nunca
habla DNS multidifusión por sí mismo: el servicio de descubrimiento pregunta
al segmento en su nombre y admite cada petición según la autoridad propia del
programa.

Con `-B` explora las instancias de un tipo de servicio, como `_ipp._tcp` para
las impresoras, y muestra cada instancia cuando se añade o se retira. Sin
tipo, enumera todos los tipos de servicio que ofrece el enlace. Con `-L`
resuelve una instancia al host y puerto en que se alcanza y muestra lo que la
instancia dice de sí misma (sus atributos `TXT`). Con `-G` busca las
direcciones de un host bajo `local`.

Cada respuesta nombra la interfaz en la que se aprendió, y una línea `Flush`
significa que todo lo aprendido en esa interfaz ya no se conoce — su enlace
cayó o el servicio volvió a empezar. Cada nombre mostrado lo eligió otra
máquina del enlace, por lo que se muestra escapado, en forma de presentación
DNS: un espacio como `\032`, un carácter de control por su código decimal.

Explorar todos los tipos, o un tipo que la cuenta en uso no tiene concedido,
requiere `CAP_NET_DISCOVER_ALL`, que solo lleva una cuenta de administrador. El
único dominio del enlace es `local`.

## OPTIONS

- `-B` — explorar las instancias de un tipo de servicio, o todos los tipos si
  no se da ninguno.
- `-L` — resolver una instancia de un tipo de servicio.
- `-G` — buscar las direcciones IPv4 (`v4`), IPv6 (`v6`) o ambas (`v4v6`) de
  un host.
- `-t` — detenerse tras ese número de segundos en lugar de ejecutarse hasta
  que se interrumpa.
- `-?, --help` — mostrar la ayuda breve propia de esta orden.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — las impresoras del enlace, según van y vienen.
- `dns-sd -t 5 -B` — todos los tipos de servicio vistos en cinco segundos.
- `dns-sd -L "Hall Printer" _ipp._tcp` — dónde se alcanza una impresora.
- `dns-sd -G v4v6 printer.local` — las direcciones de un host.

## EXIT STATUS

- `0` — la orden se completó (o se escribió la ayuda breve).
- `1` — el servicio de descubrimiento rechazó la petición o no está en
  marcha.
- `2` — no se entendió la línea de órdenes, o no se pudo escribir la salida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
