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
