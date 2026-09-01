## NAME

servicectl — iniciar, detener, habilitar y deshabilitar servicios del sistema

## SYNOPSIS

`servicectl [-h | -?] start|stop|enable|disable SERVICE`

## DESCRIPTION

Pide al gestor de servicios que cambie el estado de ejecución de un
servicio registrado, a través de su punto final de control protegido por
capacidad. Decide el gestor: esta herramienta solo codifica la petición e
informa de la respuesta.

Alcanzar el punto final es en sí mismo la autorización. Sin
`CAP_SERVICE_CONTROL` en el techo de su cuenta, el núcleo rechaza la
llamada antes de que el gestor la vea; una cuenta sin privilegios no puede
ni preguntar.

- `start SERVICE` — levantar ahora un servicio registrado que está
  detenido. Las condiciones de disponibilidad que exige siguen
  aplicándose: un servicio cuyas condiciones no se cumplen se rechaza en
  lugar de arrancarse en un sistema que no puede sostenerlo.
- `stop SERVICE` — detener con gracia un servicio en ejecución, y sus
  dependientes en orden inverso de dependencia. Se pide al servicio que
  termine y solo se fuerza tras su periodo de gracia.
- `enable SERVICE` — registrar el servicio como inscrito, para que el gestor
  lo arranque en cada arranque, y arrancarlo ahora.
- `disable SERVICE` — registrarlo como no inscrito, para que ningún arranque
  posterior lo inicie, y detenerlo ahora.

En caso de éxito, una línea nombra el estado en que el gestor dejó el
servicio.

Ambas clases de cambio afectan a todos los principales de la máquina, no
solo a su sesión. `start` y `stop` cambian solo el sistema *en ejecución*,
por lo que un servicio inscrito vuelve en el siguiente arranque; `enable` y
`disable` cambian la propia inscripción y por tanto lo sobreviven.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden y salir.
- `--` — terminar las opciones, para que un servicio cuyo nombre empieza
  por guion pueda nombrarse igualmente.

## EXIT STATUS

- `0` — la operación se aplicó, o se mostró la ayuda breve.
- `1` — el gestor rechazó la operación, o no se pudo alcanzar el punto
  final de control.
- `2` — no se entendió la línea de órdenes; no se envió nada.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `ps`
- `man`
