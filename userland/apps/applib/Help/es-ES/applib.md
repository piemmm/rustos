## NAME

applib — administrar la biblioteca de programas del escritorio

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Administra la biblioteca de programas: el catálogo organizado en
carpetas de aplicaciones ejecutables que presenta el lanzador del
escritorio. La biblioteca son datos en el volumen, nunca una lista
compilada: un almacén para toda la máquina en
`/System/Settings/ProgramLibrary/library.conf` que cada cuenta lee,
más una superposición opcional por usuario en la misma ruta dentro de
los propios `Settings/` del usuario. Lo que muestra un lanzador es el
resultado de resolver ambos juntos: las entradas y ajustes del propio
usuario prevalecen sobre los de toda la máquina.

Sin subcomando (o con `list`), la biblioteca resuelta se imprime
carpeta por carpeta, una entrada por línea: identificador, nombre
mostrado y ruta del paquete, exactamente lo que muestra el lanzador.
Las carpetas son el conjunto cerrado `Accessories`, `Graphics`,
`Internet`, `Multimedia`, `Office`, `Programming`, `Games`,
`SystemTools`, `Utilities` y `Other`; no hay carpetas de formato libre.

`applib add` registra un paquete de aplicación. Su identidad, nombre
mostrado, carpeta e icono se toman del propio manifiesto `AppInfo`
firmado del paquete; `--category`, `--name` y `--icon` anulan el
manifiesto. Un paquete cuyo manifiesto no declara ninguna carpeta de
biblioteca necesita una `--category` explícita; la herramienta nunca
adivina. `applib remove` elimina un registro, nombrado por su
identificador o por la ruta del paquete con la que fue registrado.

`applib hide` suprime una entrada de la biblioteca resuelta sin
eliminar su registro (su identificador permanece reclamado, por lo que
un `rescan` posterior no puede resucitarlo) y `applib show` vuelve a
mostrarlo. Ocultar es presentación, nunca autoridad: la ejecución de un
paquete sigue regida por las comprobaciones de firma y capacidad del
cargador independientemente del catálogo.

`applib rescan` recorre los almacenes de aplicaciones
(`/System/Commands`, `/System/Applications` y `/Apps`, o los propios
`<home>/Commands` y `<home>/Applications` del llamador con `--user`),
lee el manifiesto de cada paquete y registra cada aplicación que
solicita ser listada y aún no está catalogada. Los registros
existentes, incluidos los renombramientos y supresiones de un curador,
nunca se alteran, y un paquete con un manifiesto ilegible o malformado
se omite y se cuenta, nunca es motivo para abortar. Así es como la
biblioteca de un sistema nuevo se puebla a sí misma a partir de los
paquetes realmente instalados, sin ninguna lista mantenida a mano en
ninguna parte.

Por defecto, la herramienta edita el almacén de toda la máquina, que
solo puede cambiar un principal admitido por la política de escritura
de `/System/Settings`; una cuenta ordinaria lo lee pero lo personaliza
a través de su propia superposición con `--user`. Una escritura
denegada indica su motivo y no cambia nada.

En caso de éxito, la herramienta no muestra nada en la salida
estándar; el resultado de un cambio se emite como un registro
informativo estructurado en el flujo de información estándar (fd 3),
que los scripts pueden capturar con `3>records.jsonl` y todo lo demás
puede ignorar.

## OPTIONS

- `--category <folder>` — con `list`, mostrar solo esa carpeta; con
  `add`, archivar la entrada bajo ella (anulando la declaración del
  manifiesto).
- `--name <name>` — con `add`, el nombre a mostrar en lugar del que
  figura en el manifiesto.
- `--icon <asset>` — con `add`, el recurso de icono (un nombre de
  archivo dentro del `Resources/` del paquete) en lugar del que figura
  en el manifiesto.
- `--user` — aplicar el cambio a la propia superposición del llamador
  (o, con `rescan`, recorrer los propios `<home>/Commands` y
  `<home>/Applications` del llamador) en lugar del almacén de toda la
  máquina.
- `-h, -?` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `applib` — mostrar la biblioteca resuelta, carpeta por carpeta.
- `applib list --category Games` — mostrar una sola carpeta.
- `applib add /Apps/chess.app` — registrar un paquete según lo pide su
  manifiesto.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  registrar un paquete que no declara listado, bajo una carpeta
  explícita.
- `applib remove os.tairix.chess` — eliminar una entrada por
  identificador.
- `applib hide os.tairix.chess --user` — ocultarla solo de su propia
  biblioteca.
- `applib rescan` — registrar cada paquete instalado y listado que aún
  no esté en el catálogo de la máquina.

## EXIT STATUS

- `0` — se completó el listado, cambio, rescan o ayuda corta.
- `1` — fallo de almacén, paquete o salida (por ejemplo, el llamador no
  puede cambiar el catálogo de toda la máquina); el motivo se indica en
  el flujo de diagnóstico.
- `2` — no se entendió la línea de comandos, la carpeta o entrada es
  desconocida, o el paquete no puede registrarse según lo solicitado.

## ENVIRONMENT

- `LANG` — el locale preferido para la ayuda corta (una etiqueta BCP-47
  como `fr-FR`).
- `HOME` — el directorio personal del llamador: nombra la superposición
  por usuario y las raíces de rescan con `--user` `<home>/Commands` y
  `<home>/Applications`.

## SEE ALSO

- `man`
- `configure`
