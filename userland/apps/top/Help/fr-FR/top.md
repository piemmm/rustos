## NAME

top — observer la liste des processus en direct

## SYNOPSIS

`top [-h | -?]`

## DESCRIPTION

Affiche une vue plein écran, en direct, de la liste des processus via
l'API d'information système, dans l'esprit du `top` classique. Il
démarre sur les processus de l'appelant ; la vue système n'est accordée
par le service qu'à un appelant détenant `CAP_SYSINFO_GLOBAL`.

Le visualiseur ne prend aucun opérande : il se pilote avec des touches
pressées dans la session.

- `q` — quitter.
- `a` — basculer entre vos propres processus et la vue système. Si le
  service refuse la vue système (elle exige `CAP_SYSINFO_GLOBAL`), le
  visualiseur reste sur vos propres processus et la ligne d'état en
  indique la raison ; la session continue.
- `r` — rafraîchir la liste.
- Haut/Bas, PageHaut/PageBas, Début/Fin — déplacer la sélection.
- `h`, `?` — afficher ou masquer l'aide-mémoire des touches.

Trois lignes de synthèse précèdent la liste : la durée de fonctionnement,
le nombre d'utilisateurs connectés et les charges moyennes sur 1/5/15
minutes ; le recensement des tâches par état ; et les chiffres mémoire en
MiB. La ligne mémoire exige `CAP_SYSINFO_KERNEL` — un appelant qui ne le
détient pas voit le refus énoncé et la session continue.

Les lignes sont triées par `%CPU`, le plus gros consommateur en tête, et
portent :

- `PID` — l'identifiant numérique du processus.
- `USER` — le nom du compte propriétaire, résolu depuis l'annuaire des
  comptes du système ; l'uid numérique le remplace quand le nom ne peut
  pas être résolu.
- `SIZE` — la mémoire mappée dans l'espace d'adressage du processus
  (image, pile et tas confondus).
- `S` — la lettre d'état : `R` en exécution (vert), `r` prêt, en
  attente d'un CPU (cyan), `S` endormi, `T` arrêté (jaune), `Z` zombie
  (magenta). Les couleurs n'apparaissent que sur un terminal couleur ;
  la lettre porte toujours l'état.
- `%CPU` — la part de CPU sur l'intervalle depuis le rafraîchissement
  précédent.
- `WCPU` — la part de CPU pondérée (lissée exponentiellement) entre les
  rafraîchissements, plus stable que la colonne instantanée.
- `TIME+` — le temps CPU cumulé, sous la forme
  `minutes:secondes.centièmes`.
- `COMMAND` — le nom du processus.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande et quitter. Dans
  une session en cours, les mêmes touches basculent l'aide-mémoire des
  touches.

## EXIT STATUS

- `0` — la session s'est terminée par `q`, ou l'aide courte a été
  affichée.
- `1` — le service ou le terminal a échoué ; la raison est imprimée
  sur la sortie d'erreur standard.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
