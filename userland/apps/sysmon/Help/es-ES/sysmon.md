## NAME

sysmon — observar en vivo la memoria y la carga del núcleo

## SYNOPSIS

`sysmon [-d seg.décimas] [-h | -?]`

## DESCRIPTION

Muestra a pantalla completa, en vivo, la memoria y la carga del núcleo
a través de la API de información del sistema: memoria física, el
montículo del núcleo, la banda de presión de memoria con su historial,
el registro de cachés recuperables, el nivel comprimido `ramzip`, el
total de memoria fijada, la carga por CPU y un censo de procesos. La
herramienta sigue siendo utilizable bajo carga deliberada y reposa
entre refrescos cuando el sistema está ocioso.

Al arrancar, el monitor fija su propia memoria (`mem_pin`, que requiere
`CAP_MEM_PIN`) para no detenerse nunca en sus propios fallos de página
bajo la misma presión que observa. Una fijación rechazada se informa en
la línea de título y la sesión continúa sin fijar — la fijación es
accesoria, nunca fatal.

La pantalla se refresca en cada intervalo (3,0 segundos salvo que `-d`
lo cambie), y `r` la refresca de inmediato. El monitor no acepta
operandos: se controla con teclas dentro de la sesión.

- `q` — salir.
- `p` — alternar el panel de detalle: cachés recuperables, nivel
  comprimido, carga por CPU, procesos.
- `r` — refrescar ahora.
- `+` / `-` — alargar / acortar el intervalo en un segundo, entre 0,1
  y 60 segundos.
- Arriba/Abajo, RePág/AvPág, Inicio/Fin — desplazar el panel.
- `h`, `?` — mostrar u ocultar el resumen de teclas.

Seis líneas de resumen preceden al panel de detalle: el título (tiempo
de actividad, medias de carga y estado de fijación); las cifras de
memoria en MiB con el total fijado; la banda de presión con su
indicador, cifras de libre/reserva y contadores de entrada; el
historial de bandas (un glifo por refresco: `.` normal, `-` leve, `=`
moderada, `#` severa, `!` crítica); la línea global de CPU; y el censo
de tareas.

Cada cifra viaja por la API de información del sistema — no hay
`/proc`. Las consultas de estadísticas del núcleo requieren
`CAP_SYSINFO_KERNEL`, y el censo de todos los procesos
`CAP_SYSINFO_GLOBAL`: quien carezca de una ve el rechazo de ese panel
explicado mientras el resto de la sesión continúa. La lista interactiva
completa de procesos es tarea de `top`; el panel de procesos muestra
aquí solo el censo y los mayores consumidores por `%CPU` y por memoria.

## OPTIONS

- `-d, --delay <seconds>` — el intervalo entre refrescos automáticos,
  en segundos con fracción opcional (solo se conserva el primer dígito
  decimal, las décimas): `sysmon -d 1.5` refresca cada 1,5 segundos.
  Por defecto 3,0. GNU `top` acepta un intervalo cero y refresca tan
  rápido como puede; TAIRiX nunca gira en vacío, así que un cero se
  eleva al mínimo de 0,1 s.
- `-h, -?` — mostrar la ayuda breve de esta orden y salir. Dentro de
  una sesión en marcha, las mismas teclas alternan el resumen de
  teclas.

## EXIT STATUS

- `0` — la sesión terminó con `q`, o se mostró la ayuda breve.
- `1` — el terminal falló; la razón se escribe en la salida de error.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
