## NAME

datetime — régler la date et l'heure de la machine

## SYNOPSIS

`datetime`

## DESCRIPTION

Ouvre une fenêtre de bureau affichant l'horloge de la machine dans six
champs modifiables — année, mois et jour sur la première ligne, heure,
minute et seconde sur la seconde — et règle l'horloge sur ce qu'ils
indiquent. Rien ne change avant l'appui sur **Set**.

L'affichage est en UTC. TAIRiX ne conserve aucun décalage de fuseau
horaire : il n'y a donc pas d'heure locale à afficher ni à saisir.

La fenêtre s'atteint normalement depuis le menu de l'horloge du bureau :
cliquer sur l'horloge dans la barre d'icônes et choisir **Set Date &
Time…**. Régler l'horloge exige une autorité qu'une session de bureau ne
possède pas ; le bureau demande donc un compte qui la possède, et cette
application est démarrée en tant que ce compte une fois le mot de passe
accepté.

Cliquer sur un champ pour y saisir, ou appuyer sur `Tab` pour passer au
suivant. Seuls les chiffres sont acceptés, avec un `-` initial autorisé
dans l'année pour une date antérieure à l'an 1. `Enter` règle l'horloge ;
`Escape` ferme la fenêtre.

Chaque champ est vérifié avant tout réglage, et le premier défaut est
énoncé dans la fenêtre plutôt que corrigé en silence : un mois hors de 1
à 12, une heure hors de 0 à 23, une minute ou une seconde hors de 0 à 59,
ou un jour qui n'existe pas dans le mois et l'année saisis — le 31 avril,
ou le 29 février hors d'une année bissextile. Rien n'est réglé lorsqu'un
champ est refusé.

Les dates antérieures à 1970 et bien postérieures à 2038 sont des
saisies ordinaires. L'horloge est une valeur signée sur 64 bits : ni
l'une ni l'autre n'est une limite.

Si l'horloge de la machine n'a jamais été réglée depuis son démarrage,
les champs s'ouvrent **vides** et la fenêtre le dit. Ils ne sont pas
remplis avec l'époque Unix, qui serait une date que la machine n'a jamais
revendiquée.

Si le compte sous lequel cette application s'exécute ne peut pas régler
l'horloge, la tentative est refusée, la fenêtre le dit, et l'horloge
reste exactement telle qu'elle était. La raison est également écrite sur
le flux d'erreur standard. L'application continue de fonctionner : un
réglage refusé est une réponse, pas une défaillance du programme.

## EXIT STATUS

Zéro après une fermeture propre, y compris lorsqu'un réglage a été
refusé. Non nul lorsque la fenêtre n'a pu être ouverte, que la région de
trame partagée a été refusée ou que le canal de fenêtre a été perdu ; la
raison est énoncée sur le flux d'erreur standard.

## SEE ALSO

`sysinfo`, `uptime`
