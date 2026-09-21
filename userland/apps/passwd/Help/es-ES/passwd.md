## NAME

passwd — fijar la contraseña de una cuenta

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Sustituye la contraseña almacenada de la cuenta nombrada. Fijar una
contraseña es una operación administrativa: la base rechaza a quien no posea
la capacidad de administración de usuarios.

Ninguna contraseña en claro cruza la llamada al sistema. La herramienta
pregunta dos veces con el eco del terminal apagado, resume lo tecleado en un
registro PBKDF2 con sal — tomada de la fuente aleatoria del núcleo — y envía
el registro; ambos búferes en claro se ponen a cero en cuanto éste existe.

El nombre de la cuenta es obligatorio. El `passwd` de GNU sin operando
cambia la contraseña del propio llamante, lo que en TAIRiX exigiría una vía
de autoservicio sin privilegios que no existe: toda la interfaz de
administración de cuentas está protegida por capacidad, y exceptuar «tu
propio registro» sería un cambio del modelo de seguridad, no una comodidad.

Un llamante sin terminal — un programa gráfico, cuya entrada estándar está
cerrada bajo el intermediario de elevación — resume él mismo la contraseña y
entrega el registro terminado con `--record`, de modo que no existe claro en
ninguno de los dos lados. El registro se comprueba como bien formado antes
de almacenarse.

`--` termina el análisis de opciones: todo argumento posterior es un operando.

## OPTIONS

- `--record RECORD` — un registro PBKDF2 con sal ya preparado, para un
  llamante sin terminal donde preguntar.
- `-h, -?, --help` — mostrar la ayuda breve propia de esta orden.

## EXAMPLES

- `passwd ada` — preguntar dos veces y fijar la contraseña de la cuenta.

## EXIT STATUS

- `0` — la contraseña fue sustituida.
- `1` — la base rechazó o no pudo completar la sustitución, las dos entradas
  no coincidieron, no había aleatoriedad disponible, o el registro estaba
  mal formado; la razón se imprime en la salida de error.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
