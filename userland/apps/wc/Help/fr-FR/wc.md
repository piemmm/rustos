## NAME

wc — afficher le nombre de lignes, de mots et d'octets de chaque fichier

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

Compte, pour chaque `file`, ses lignes (caractères de saut de ligne),
ses mots et ses octets, et les affiche sur une ligne suivie du nom du
fichier. Sans `file`, ou quand `file` vaut `-`, l'entrée standard est
lue (et aucun nom n'est affiché pour la forme sans opérande). Avec
plusieurs entrées, une ligne finale `total` est affichée selon
`--total`.

Les sélecteurs `-l`, `-w`, `-m`, `-c` et `-L` choisissent les comptes
affichés ; sans aucun, les comptes de lignes, de mots et d'octets sont
affichés. Les comptes apparaissent toujours dans l'ordre fixe :
lignes, mots, caractères, octets, largeur maximale de ligne. Un mot est
une suite maximale de caractères non blancs. `-m` compte les caractères
UTF-8 (un octet qui n'est pas de l'UTF-8 valide compte comme octet
mais pas comme caractère) ; `-L` mesure la largeur d'affichage de
chaque ligne en colonnes de terminal, les tabulations avançant au
prochain multiple de 8.

`--files0-from <file>` lit la liste des opérandes, séparés par NUL,
depuis `file` (`-` signifie l'entrée standard) ; elle ne peut pas être
combinée avec des opérandes `file`.

Une entrée illisible est signalée sur la sortie d'erreur et
l'exécution continue avec l'entrée suivante.

## OPTIONS

- `-c, --bytes` — afficher le nombre d'octets.
- `-m, --chars` — afficher le nombre de caractères.
- `-l, --lines` — afficher le nombre de sauts de ligne.
- `-w, --words` — afficher le nombre de mots.
- `-L, --max-line-length` — afficher la largeur d'affichage maximale
  d'une ligne.
- `--files0-from <file>` — lire la liste des opérandes séparés par NUL
  depuis `file` (`-` la lit depuis l'entrée standard).
- `--total <when>` — quand afficher la ligne `total` : `auto` (par
  défaut : seulement avec plusieurs entrées), `always`, `only`
  (seulement le total, sans étiquette) ou `never`.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `wc notes.txt` — afficher les comptes de lignes, de mots et d'octets
  de `notes.txt`.
- `wc -l a b` — afficher le nombre de lignes de `a` et de `b`, puis le
  total.
- `wc -L table.txt` — afficher la ligne la plus large de `table.txt`
  en colonnes de terminal.
- `wc -c --total=only a b` — afficher seulement la somme des octets.

## EXIT STATUS

- `0` — chaque entrée a été comptée (ou l'aide courte a été écrite).
- `1` — une entrée n'a pas pu être lue, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `cat`
- `head`
- `man`
