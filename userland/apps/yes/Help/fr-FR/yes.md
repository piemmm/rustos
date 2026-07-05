## NAME

yes — écrire une ligne de texte en boucle

## SYNOPSIS

`yes [chaîne...]`

## DESCRIPTION

Écrit ses opérandes, joints par des espaces simples — ou `y` s'il n'y en
a aucun — suivis d'un saut de ligne, encore et encore, jusqu'à ce que sa
sortie n'accepte plus d'octets (un tube fermé) ou que le processus soit
terminé. Son rôle historique est de fournir une réponse affirmative à
une commande qui pose des questions ; son rôle moderne est d'être une
source bon marché de texte répété.

L'analyse des options s'arrête au premier opérande : `yes a -x` écrit
`a -x`. Une option inconnue avant les opérandes est une erreur ; écrire
`yes -- -x` pour imprimer une chaîne qui ressemble à une option.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.
- `--` — terminer l'analyse des options ; tout argument ultérieur est
  un opérande.

## EXAMPLES

- `yes` — écrire `y` jusqu'à interruption.
- `yes hello world` — écrire `hello world` jusqu'à interruption.
- `yes -- -x` — écrire `-x` (après `--`, les opérandes peuvent
  ressembler à des options).

## EXIT STATUS

- `0` — l'aide courte demandée a été servie.
- `1` — la sortie n'accepte plus d'octets (la seule condition d'arrêt
  de l'outil).
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `true`
- `man`
