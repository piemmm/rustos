## NAME

df — rendre compte de l'occupation des systèmes de fichiers

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Affiche, une ligne par système de fichiers monté, la taille du volume,
l'espace utilisé, l'espace disponible, le pourcentage d'utilisation et
le point de montage. Avec des opérandes `file`, c'est le système de
fichiers contenant chaque opérande qui est rapporté (une ligne par
système de fichiers, quel que soit le nombre d'opérandes couverts).

Les chiffres proviennent de la liste des montages de l'API
d'informations système, telle que chaque pilote de système de fichiers
monté rapporte sa propre comptabilité. Par défaut, le rapport masque
les montages sans capacité propre (les liaisons de vue synthétiques du
système) et les montages supplémentaires d'un volume déjà listé ; `-a`
montre tout, et le nombre d'entrées masquées est noté sur le flux
d'information standard (fd 3), jamais dans la table.

Les tailles sont affichées en blocs de 1024 octets sauf si une option
d'unité en décide autrement ; une option d'unité ultérieure remplace
la précédente, et les nombres de blocs sont arrondis vers le haut. Un
système de fichiers dont le format alloue les inœuds à la demande
rapporte des chiffres d'inœuds nuls sous `-i` — la réponse honnête
« non suivi ».

Un opérande `file` qui n'existe pas, ou qui est un chemin relatif
(les points de montage sont absolus ; `df` ne devine jamais une
résolution), est signalé sur la sortie d'erreur standard et le rapport
continue avec le reste. Les options GNU `--output`, `--sync` et
`--no-sync` ne sont pas encore disponibles.

## OPTIONS

- `-a, --all` — inclure les montages sans capacité et en double que
  le comportement par défaut masque.
- `-T, --print-type` — ajouter la colonne du type de système de
  fichiers.
- `-t, --type <type>` — ne rapporter que les systèmes de fichiers du
  type `type` (répétable).
- `-x, --exclude-type <type>` — omettre les systèmes de fichiers du
  type `type` (répétable).
- `-i, --inodes` — rapporter les comptes d'inœuds au lieu de
  l'occupation en blocs.
- `-P, --portability` — le format portable POSIX (en-têtes
  `1024-blocks` et `Capacity`).
- `-l, --local` — restreindre le rapport aux systèmes de fichiers
  locaux (tous les montages RustOS aujourd'hui : rien n'est filtré).
- `--total` — ajouter une ligne étiquetée `total` sommant les
  chiffres affichés.
- `-k` — blocs de 1024 octets (la valeur par défaut).
- `-h, --human-readable` — tailles lisibles en puissances de 1024
  (`1.0K`, `23M`).
- `-H, --si` — tailles lisibles en puissances de 1000 (`1.0k`,
  `23M`).
- `-B, --block-size <size>` — rapporter en blocs de `size` octets
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `df` — l'occupation de chaque volume réel en blocs de 1024 octets.
- `df -h` — la même chose, en tailles lisibles.
- `df /Users/jo` — le système de fichiers contenant `/Users/jo`.
- `df -aT` — chaque montage, avec son type de système de fichiers.
- `df --total -k` — les volumes plus une ligne `total` sommée.

## EXIT STATUS

- `0` — le rapport a couvert tout ce qui était demandé (ou l'aide
  courte a été écrite).
- `1` — un opérande n'a pas pu être rapporté, les filtres n'ont rien
  laissé, ou la requête/la sortie a échoué.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la langue préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
