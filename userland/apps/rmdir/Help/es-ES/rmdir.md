## NAME

rmdir — eliminar directorios vacíos

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directorio...`

## DESCRIPTION

Elimina cada directorio operando, en orden. Solo se elimina un
**directorio vacío**: el propio sistema de ficheros rechaza un fichero
(o cualquier otro objeto) y un directorio con contenido, de forma
atómica, de modo que nunca puede eliminarse otra cosa en su lugar. Use
`rm` para ficheros y `rm -r` para árboles con contenido.

Con `-p` también se eliminan los ancestros de cada operando, del más
interno al más externo: `rmdir -p a/b/c` elimina `a/b/c`, luego `a/b`
y luego `a`. La raíz desnuda de una ruta (`/` o una raíz de alias como
`Home:/`) nunca se solicita.

Con `--ignore-fail-on-non-empty` un rechazo «directorio no vacío» no
es un error: el operando (o el ascenso de `-p`) simplemente se detiene
ahí. Ningún otro rechazo se tolera. El primer fallo real detiene la
ejecución antes de cualquier operando posterior. `--` termina el
análisis de opciones: cada argumento posterior es una ruta.

## OPTIONS

- `-p, --parents` — eliminar también los ancestros de cada operando,
  del más interno al más externo.
- `-v, --verbose` — informar de cada intento de eliminación como
  `rmdir: removing directory, 'dir'`.
- `--ignore-fail-on-non-empty` — un directorio no vacío no es un
  error; con `-p` el ascenso se detiene ahí.
- `-h, -?` — mostrar la ayuda corta de esta orden (también `--help`).

## EXAMPLES

- `rmdir Scratch` — eliminar un directorio vacío.
- `rmdir -p Projects/os/build` — eliminar la cadena, del más interno
  al más externo.
- `rmdir -p --ignore-fail-on-non-empty a/b` — eliminar `a/b`, y
  también `a` si con ello queda vacío.

## EXIT STATUS

- `0` — todas las eliminaciones tuvieron éxito (un rechazo tolerado
  por `--ignore-fail-on-non-empty` no es un fallo).
- `1` — un fallo del sistema de ficheros o de la salida; la razón se
  imprime en la salida de error.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

mkdir, rm, ls
