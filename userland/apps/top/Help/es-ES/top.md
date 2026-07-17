## NAME

top — observar la lista de procesos en vivo

## SYNOPSIS

`top [-d segs.décimas] [-h | -?]`

## DESCRIPTION

Muestra una vista en vivo, a pantalla completa, de la lista de procesos
a través de la API de información del sistema, al estilo del `top`
clásico. Comienza con los procesos del llamante; el servicio concede la
vista de todo el sistema únicamente a un llamante que posea
`CAP_SYSINFO_GLOBAL`.

La pantalla se refresca sola en cada intervalo (3,0 segundos salvo que
`-d` lo cambie), y `r` la refresca de inmediato.

El visor no acepta operandos: se controla con teclas pulsadas dentro de
la sesión.

- `q` — salir.
- `a` — alternar entre sus propios procesos y la vista de todo el
  sistema. Si el servicio rechaza la vista de todo el sistema (requiere
  `CAP_SYSINFO_GLOBAL`), el visor permanece en sus propios procesos y
  la línea de estado indica el motivo; la sesión continúa.
- `r` — refrescar la lista.
- Arriba/Abajo, RePág/AvPág, Inicio/Fin — mover la selección.
- `h`, `?` — mostrar u ocultar el resumen de teclas.

Cuatro líneas de resumen preceden a la lista: el tiempo de actividad, el
número de usuarios conectados y las cargas medias de 1/5/15 minutos; el
censo de tareas por estado; el reparto de uso `%Cpu(s)`; y las cifras de
memoria en MiB. La línea de memoria exige `CAP_SYSINFO_KERNEL`: quien no
lo posea ve el rechazo explicado y la sesión continúa.

La línea `%Cpu(s)` muestra la parte del último intervalo que todas las
CPU juntas pasaron ocupadas (ejecutando tareas) e inactivas. TAIRiX solo
contabiliza tiempo ocupado e inactivo: donde el `top` de GNU desglosa la
parte ocupada en usuario/sistema/nice/iowait, esta línea muestra
deliberadamente las dos cifras reales.

Las filas se ordenan por `%CPU`, el mayor consumidor primero, y llevan:

- `PID` — el identificador numérico del proceso.
- `USER` — el nombre de la cuenta propietaria, resuelto desde el
  directorio de cuentas del sistema; el uid numérico lo sustituye cuando
  el nombre no puede resolverse.
- `SIZE` — la memoria mapeada en el espacio de direcciones del proceso
  (imagen, pila y montón por igual).
- `S` — la letra de estado: `R` en ejecución (verde), `r` listo, a la
  espera de una CPU (cian), `S` durmiendo, `T` detenido (amarillo), `Z`
  zombi (magenta). Los colores solo aparecen en un terminal en color; la
  letra siempre porta el estado.
- `%CPU` — la cuota de CPU en el intervalo desde el refresco anterior.
- `WCPU` — la cuota de CPU ponderada (suavizada exponencialmente) entre
  refrescos, más estable que la columna instantánea.
- `TIME+` — el tiempo de CPU acumulado, como
  `minutos:segundos.centésimas`.
- `COMMAND` — el nombre del proceso.

## OPTIONS

- `-d, --delay <seconds>` — el intervalo entre refrescos automáticos,
  en segundos con fracción opcional (solo se conserva el primer dígito
  decimal, las décimas): `top -d 1.5` refresca cada 1,5 segundos. Por
  defecto 3,0. El `top` de GNU acepta un retardo cero y refresca tan
  rápido como puede; TAIRiX nunca entra en un bucle activo, así que un
  cero se eleva al mínimo de 0,1 s.
- `-h, -?` — mostrar la ayuda corta de este comando y salir. Dentro de
  una sesión en curso, las mismas teclas alternan el resumen de teclas.

## EXIT STATUS

- `0` — la sesión terminó con `q`, o se mostró la ayuda corta.
- `1` — el servicio o el terminal falló; el motivo se imprime en la
  salida de error estándar.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
