## NAME

host — resolver un nombre por DNS

## SYNOPSIS

`host [-t type] name`

## DESCRIPTION

Resuelve un nombre de dominio en sus direcciones usando el resolvedor básico
del sistema e imprime cada respuesta, una por línea. Sin `-t` se consultan
tanto los registros `A` (IPv4) como `AAAA` (IPv6); `-t type` limita la
consulta a uno.

Los servidores DNS recursivos que consultar se leen de la configuración del
host a través de la API de información del sistema — el mismo conjunto activo
que muestra la lectura `state:net/resolver/servers` — y cada respuesta se
valida antes de mostrar una dirección. No hay `/etc/resolv.conf` ni archivo
de hosts local.

Solo se admiten los registros de dirección `A` y `AAAA`; los demás tipos
(`MX`, `TXT`, etc.) se rechazan en lugar de tratarse silenciosamente como
`A`. Un nombre que no existe imprime `Host <name> not found: 3(NXDOMAIN)`;
cuando no se puede alcanzar ningún servidor, `host` informa de un tiempo de
espera agotado en la salida de error.

## OPTIONS

- `-t, --type` — el tipo de registro DNS que consultar: `A` o `AAAA` (no
  distingue mayúsculas). Sin esta opción se consultan ambos.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `host example.com` — las direcciones IPv4 e IPv6 del nombre.
- `host -t AAAA example.com` — solo las direcciones IPv6.

## EXIT STATUS

- `0` — se encontró al menos una dirección (o se escribió la ayuda breve).
- `1` — el nombre no resolvió ninguna dirección (respuesta negativa, tiempo
  de espera agotado o fallo del resolvedor).
- `2` — no se entendió la línea de órdenes, o no se pudo escribir la salida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
