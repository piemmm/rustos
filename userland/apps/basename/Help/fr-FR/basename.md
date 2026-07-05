## NAME

basename — retirer le répertoire et le suffixe des noms

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Affiche le dernier composant de chaque chemin : les barres obliques
finales sont retirées, puis tout ce qui précède la dernière barre
restante, celle-ci comprise. L'opération est purement lexicale — aucun
chemin n'est résolu ni touché sur le disque. Avec un `suffix` (le
second opérande, ou `-s`), un `suffix` final est également retiré, sauf
s'il constitue tout le nom restant.

Une racine n'est jamais entamée : `basename /` donne `/`, et —
l'équivalent dans la forêt de stockage RustOS — `basename Home:/` donne
`Home:/`. Une racine d'alias (`Home:/`, `System:/`, …) joue exactement
le rôle que `/` joue sur les systèmes POSIX.

Sans `-a` ni `-s`, au plus deux opérandes sont acceptés : le nom et un
suffixe facultatif. Avec `-a` (ou `-s`, qui l'implique), chaque
opérande est un nom.

## OPTIONS

- `-a, --multiple` — traiter chaque opérande comme un nom.
- `-s, --suffix <suffix>` — retirer un `suffix` final de chaque nom ;
  implique `-a`. S'écrit aussi `--suffix=<suffix>` ou groupé (`-s.rs`).
- `-z, --zero` — terminer chaque résultat par NUL au lieu d'un saut de
  ligne.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `basename /System/Apps/top.app` — afficher `top.app`.
- `basename src/lib.rs .rs` — afficher `lib`.
- `basename -s .rs -a a.rs b.rs` — afficher `a` puis `b`.
- `basename Home:/` — afficher `Home:/` (une racine n'est jamais
  entamée).

## EXIT STATUS

- `0` — les résultats (ou l'aide courte) ont été écrits.
- `1` — la sortie n'a pas pu être délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `dirname`
- `man`
