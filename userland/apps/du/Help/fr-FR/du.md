## NAME

du — estimer l'espace disque occupé par les fichiers

## SYNOPSIS

`du [option...] [file...]`

## DESCRIPTION

Parcourt chaque opérande `file` et affiche, par répertoire (le plus
profond d'abord), l'espace de stockage occupé par l'arborescence qui
se trouve dessous, sous la forme `size<TAB>path`. Sans `file`, le
répertoire courant (`.`) est parcouru. Un opérande `file` qui n'est
pas un répertoire est affiché seul.

La mesure par défaut est le stockage réellement alloué de chaque
nœud, tel que le système de fichiers monté le rapporte ; les fichiers
creux ou compressés comptent donc ce qu'ils occupent réellement.
`--apparent-size` (ou `-b`) mesure à la place les longueurs
apparentes en octets. Les tailles sont affichées en blocs de 1024
octets sauf si une option d'unité en décide autrement ; une option
d'unité ultérieure remplace la précédente, et les nombres de blocs
sont arrondis vers le haut (un bloc partiellement utilisé est un bloc
utilisé).

Un chemin illisible est signalé sur la sortie d'erreur standard et le
parcours continue avec le reste ; un répertoire illisible ne
contribue rien plutôt qu'une somme partielle devinée.

`du` ne déduplique pas encore un fichier portant plusieurs noms : celui
atteint par deux noms est compté une fois par nom, et les options GNU de
déduplication de liens n'existent pas ; `-x` (un seul système de fichiers) n'est pas encore
disponible ; les variables d'environnement de la famille
`DU_BLOCK_SIZE` ne sont pas lues — l'échelle est choisie par les
options seules.

## OPTIONS

- `-a, --all` — afficher aussi chaque fichier, pas seulement les
  répertoires.
- `-s, --summarize` — n'afficher que le total de chaque opérande (en
  conflit avec `-a` et `-d`).
- `-c, --total` — ajouter une ligne de total général étiquetée
  `total`.
- `-d, --max-depth <n>` — afficher les répertoires jusqu'à `n`
  niveaux sous un opérande (`0` n'affiche que les opérandes) ; les
  totaux ne changent pas.
- `-S, --separate-dirs` — la ligne d'un répertoire exclut ses
  sous-répertoires.
- `--apparent-size` — mesurer les longueurs apparentes en octets, pas
  le stockage alloué.
- `-b, --bytes` — taille apparente en octets simples
  (`--apparent-size` avec une taille de bloc de 1).
- `-k` — blocs de 1024 octets (la valeur par défaut).
- `-m` — blocs de 1048576 octets.
- `-h, --human-readable` — tailles lisibles en puissances de 1024
  (`1.0K`, `23M`).
- `--si` — tailles lisibles en puissances de 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — afficher en blocs de `size` octets
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-0, --null` — terminer chaque ligne par NUL au lieu d'un saut de
  ligne.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `du` — l'arborescence du répertoire courant, une ligne par
  répertoire.
- `du -sh /Users/jo` — un total lisible pour `/Users/jo`.
- `du -a docs` — chaque fichier et répertoire sous `docs`.
- `du -d1 -c /Apps /Users` — le premier niveau de chaque magasin,
  puis un total général.

## EXIT STATUS

- `0` — chaque opérande a été parcouru (ou l'aide courte a été
  écrite).
- `1` — un chemin n'a pas pu être lu, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la langue préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `df`
- `ls`
- `man`
