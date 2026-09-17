## NAME

cinder — un compañero de escritorio que vive en un corralito y recorre el escritorio

## SYNOPSIS

`cinder`

## DESCRIPTION

Abre una pequeña ventana de corralito con Cinder dentro. Cinder es la mascota de
TAIRiX: una criatura pequeña, de color óxido y carbón, que deambula, se acicala,
dormita y se fija en dónde está el puntero.

Se le puede dejar salir. Elija **Dejar salir a Cinder** en su menú de la barra de
iconos y abandonará el corralito para ir al escritorio, donde pasea entre sus
ventanas. Al encontrarse con una, trepará y se sentará en su barra de título, se
aplastará para colarse por debajo, o simplemente la rodeará: lo que haga depende
de la forma de la ventana y de su humor. Acerque el puntero y lo mirará, lo
seguirá al trote y se abalanzará sobre él si lo alcanza.

Haga clic sobre él para acariciarlo, dentro del corralito o fuera. Las caricias
le animan. Fuera, solo él recibe el clic: el espacio transparente a su alrededor
pertenece a lo que haya detrás, de modo que un compañero sentado sobre su
trabajo nunca se traga un clic dirigido a este.

En el corralito también puede cogerlo y dejarlo en cualquier punto del suelo, y
darle golpecitos a la pelota.

**El menú.** **Dejar salir a Cinder** / **Traer a Cinder a casa** cambia dónde está.

**Salir** lo termina y lo retira del escritorio.

**Cerrar el corralito.** Cerrar la ventana del corralito no termina la aplicación. Cinder es una
aplicación residente: cerrar el corralito con él dentro lo guarda, y cerrarlo
mientras está fuera lo deja vagando. Haga clic en su icono de la barra para
volver a abrir el corralito. *Salir* es lo que lo termina.

**Humor.** Cinder quiere tres cosas: descanso, juego y compañía. Correr gasta su energía y
una siesta la repone; quedarse quieto le aburre y perseguir el puntero le
entretiene; las caricias le animan. No tiene que gestionar nada de esto: no hay
nada que darle de comer y nada se estropea si lo deja tranquilo. Está ahí para
que vea de un vistazo de qué humor está.

Cómo se siente, y si estaba fuera, se recuerda entre sesiones.

**Cuando no puede salir.** Estar en el escritorio fuera de una ventana requiere la capacidad
`CAP_DESKTOP_LAYER`, que esta aplicación solicita en su manifiesto firmado y que
los permisos de su cuenta también deben permitir. Si no lo hacen —o si no hay
sesión gráfica— **Dejar salir a Cinder** indica el motivo en el corralito y en
la salida de error, y el corralito sigue funcionando. Cinder simplemente se
queda dentro.

## EXIT STATUS

`0` al salir limpiamente. Un estado distinto de cero va siempre acompañado del
motivo en la salida de error.

## SEE ALSO

`sapper`
