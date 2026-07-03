## NAME

man — mostrar el documento de ayuda de una orden

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Muestra el documento de ayuda que incluye el paquete de aplicación de una
orden, en su idioma cuando existe una traducción.

Cada programa de RustOS es un paquete de aplicación con un árbol `Help/`:
un documento estructurado por orden o tema, por idioma. `man` resuelve
`<command>` exactamente como el intérprete de órdenes — primero la tienda
de aplicaciones del sistema y después los directorios de `PATH` — de modo
que la página mostrada siempre documenta el programa que el intérprete
ejecutaría para la misma palabra. Un sufijo `.app` nombra el paquete
directamente.

El documento se elige según la configuración regional de la variable de
entorno `LANG`, con reserva hacia el mismo idioma de otra región y, por
último, hacia el documento canónico en inglés. Cuando la página no se
muestra en el idioma solicitado, `man` anota la sustitución en el flujo
consultivo (fd 3); la página en sí nunca mezcla idiomas.

En una consola interactiva la página se muestra pantalla a pantalla: la
barra espaciadora pasa de página, intro avanza una línea y `q` termina.
Cuando la salida está redirigida o se desconoce el tamaño de la consola,
la página completa fluye sin pausas.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `man ps` — mostrar la página de `ps`.
- `man top keys` — mostrar el tema `keys` del paquete `top`.
- `man files.app` — nombrar el paquete directamente.

## EXIT STATUS

- `0` — la página se mostró.
- `1` — no se encontró la orden o su documento de ayuda, o la página no
  pudo entregarse.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida (una etiqueta BCP-47 como
  `es-ES`).
- `PATH` — los directorios adicionales donde buscar paquetes
  `<command>.app`, después de la tienda de aplicaciones del sistema.

## SEE ALSO

- `elsh`
