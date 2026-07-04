## NAME

users — administrar cuentas de usuario y grupos

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Ejecuta la sesión interactiva de administración de cuentas sobre la
interfaz controlada `users_admin`. Cada operación se decide en el lado
del núcleo según su identidad atestiguada por el núcleo: sin
`CAP_USER_ADMIN` en el techo de su cuenta, toda operación se rechaza en
el despacho. Las contraseñas se leen con el eco del terminal apagado y
se convierten en un registro con sal en el lado cliente; el texto claro
nunca cruza la interfaz y nunca se muestra ni se registra.

La herramienta no acepta operandos: las cuentas se administran con
comandos escritos dentro de la sesión.

- `list` — listar las cuentas de usuario.
- `groups` — listar los grupos.
- `create <name> <uid> <gid>` — crear una cuenta.
- `passwd <name>` — establecer la contraseña de una cuenta.
- `lock <name>`, `unlock <name>` — deshabilitar o rehabilitar una
  cuenta.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — editar las
  capacidades concedidas a una cuenta.
- `deluser <name>` — eliminar una cuenta.
- `addgroup`, `delgroup` — crear o eliminar un grupo.
- `help` — listar los comandos de la sesión.
- `exit`, `quit` — terminar la sesión.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de este comando y salir.

## EXIT STATUS

- `0` — la sesión terminó limpiamente, o se mostró la ayuda corta.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
