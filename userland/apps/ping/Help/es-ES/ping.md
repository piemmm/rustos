## NAME

ping — enviar solicitudes de eco ICMP a un host de red

## SYNOPSIS

`ping [option...] host`

## DESCRIPTION

Envía solicitudes de eco ICMP (IPv4) o ICMPv6 (IPv6) a un host y muestra
cada respuesta con su tiempo de ida y vuelta, seguido de un resumen
final.

Las solicitudes fluyen por un socket de eco ICMP abierto en la pila de
red en espacio de usuario, protegido por `CAP_NET` y `CAP_NET_RAW` y
auditado. La pila posee el identificador de eco, de modo que un socket
solo recibe respuestas a sus propias solicitudes.

El destino es una dirección IPv4 o IPv6 literal o un nombre de host. Un
nombre se resuelve mediante el resolvedor del sistema, con los servidores
recursivos configurados en la máquina; una dirección literal no requiere
consulta alguna y funciona por tanto incluso sin resolvedor configurado.
Un nombre que no resuelve a ninguna dirección de la familia pedida
termina la ejecución indicando el motivo.

De forma predeterminada cada solicitud lleva datos aleatorios de alta
entropía, extraídos de nuevo en cada solicitud. Es deliberado: un enlace
que comprime o deduplica el tráfico informaría por lo demás de un
rendimiento y una latencia que nada dicen de su capacidad real. Los bytes
devueltos se comparan con los enviados, así que una carga aleatoria es
también una comprobación de integridad por paquete. Use `-p` para un
patrón fijo cuando lo que se quiere es una carga determinista.

De forma predeterminada `ping` envía una solicitud por segundo hasta
interrumpirse; `-c` acota la cantidad. Cada respuesta indica el origen, el
número de secuencia y el tiempo; una solicitud sin respuesta dentro del
plazo muestra una línea de expiración. El resumen final indica los
paquetes transmitidos y recibidos, el porcentaje de pérdida y los tiempos
de ida y vuelta mínimo, medio y máximo. `-q` solo muestra la cabecera y el
resumen.

El tiempo de vida IP no se expone por la interfaz del socket de eco; a
diferencia de algunas implementaciones de `ping`, una línea de respuesta
no lleva un campo `ttl=`.

## OPTIONS

- `-c, --count` — detenerse tras enviar esta cantidad de solicitudes.
- `-i, --interval` — segundos entre solicitudes (un decimal, p. ej. `0.5`).
- `-s, --size` — tamaño de la carga útil en bytes.
- `-p, --pattern` — contenido de la carga útil: `random` (predeterminado,
  alta entropía) o una secuencia de dígitos hexadecimales de longitud par
  como patrón de bytes repetido, p. ej. `-p ff00`.
- `-W, --timeout` — segundos de espera por cada respuesta.
- `-w, --deadline` — plazo global de la ejecución, en segundos.
- `-4, --ipv4` — exigir un destino IPv4.
- `-6, --ipv6` — exigir un destino IPv6.
- `-n, --numeric` — salida numérica. Aceptada y sin efecto: nunca se hace
  resolución inversa, así que las direcciones de respuesta ya son
  numéricas.
- `-q, --quiet` — silencioso: solo la cabecera y el resumen final.
- `-?, --help` — mostrar la ayuda breve de este comando.

## EXAMPLES

- `ping 10.0.2.2` — hacer ping a un host IPv4 hasta interrumpirse.
- `ping -c 4 fe80::1` — enviar cuatro solicitudes a un host IPv6.
- `ping -c 10 -i 0.2 10.0.0.1` — diez solicitudes, una cada 200 ms.
- `ping -q -c 100 10.0.0.1` — ejecución silenciosa, solo resumen.

## EXIT STATUS

- `0` — se recibió al menos una respuesta (o se escribió la ayuda breve).
- `1` — ninguna solicitud obtuvo respuesta.
- `2` — no se entendió la línea de órdenes, el destino no se resolvió, o
  no se pudo abrir el socket.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `host`
- `ss`
- `sysinfo`
- `man`
