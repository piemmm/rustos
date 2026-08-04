## NAME

dirname — retirer le dernier composant des noms

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Affiche chaque chemin avec son dernier composant retiré : les barres
obliques finales sont retirées, puis le dernier composant et les barres
qui le précèdent. L'opération est purement lexicale — aucun chemin
n'est résolu ni touché sur le disque. Un chemin sans barre restante a
pour parent `.` ; un parent qui se vide est la racine.

Une racine n'est jamais entamée : `dirname /tools` donne `/`, et —
l'équivalent dans la forêt de stockage TAIRiX — `dirname Home:/tools`
donne `Home:/`. Une racine d'alias (`Home:/`, `System:/`, …) joue
exactement le rôle que `/` joue sur les systèmes POSIX.

## OPTIONS

- `-z, --zero` — terminer chaque résultat par NUL au lieu d'un saut de
  ligne.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `dirname /System/Commands/top.app` — afficher `/System/Commands`.
- `dirname src/lib.rs` — afficher `src`.
- `dirname file` — afficher `.` (pas de partie répertoire).
- `dirname Home:/tools` — afficher `Home:/` (une racine n'est jamais
  entamée).

## EXIT STATUS

- `0` — les résultats (ou l'aide courte) ont été écrits.
- `1` — la sortie n'a pas pu être délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `basename`
- `man`
