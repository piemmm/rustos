## NAME

sleep — faire une pause pour la somme d'intervalles de temps

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Fait une pause pendant la somme des intervalles donnés, puis se termine.

Chaque `NUMBER` est une valeur à virgule flottante ; un `SUFFIX` d'une
seule lettre la met à l'échelle : `s` pour les secondes (par défaut), `m`
pour les minutes, `h` pour les heures et `d` pour les jours. Plusieurs
opérandes sont additionnés, donc `sleep 1m 30s` fait une pause de
quatre-vingt-dix secondes. `inf` (ou `infinity`) fait une pause jusqu'à ce
que le processus soit tué.

Contrairement au minutage propre d'un shell, `sleep` dort hors du
processeur : la tâche est mise en attente jusqu'à la fin de l'intervalle et
ne fait jamais tourner un cœur à vide.

Une valeur négative, un `nan`, un suffixe inconnu ou des caractères
supplémentaires après le nombre est un `invalid time interval`. Ne donner
aucun opérande est un `missing operand`.

Cette commande n'affiche pas de version du système ; TAIRiX n'a pas de
telle chaîne, donc — contrairement à GNU `sleep` — elle n'a pas d'option
`--version`.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.
- `--` — terminer l'analyse des options ; tout argument ultérieur est un
  opérande.

## EXAMPLES

- `sleep 5` — faire une pause de cinq secondes.
- `sleep 1.5h` — faire une pause de quatre-vingt-dix minutes.
- `sleep 1m 30s` — faire une pause de quatre-vingt-dix secondes (les
  opérandes sont additionnés).
- `sleep inf` — faire une pause jusqu'à ce que le processus soit tué.

## EXIT STATUS

- `0` — l'intervalle s'est écoulé, ou l'aide courte demandée a été écrite.
- `1` — l'écriture de l'aide courte a échoué.
- `2` — la ligne de commande n'a pas été comprise (option inconnue,
  opérande manquant ou intervalle de temps invalide).

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle
  que `fr-FR`).

## SEE ALSO

- `top`
