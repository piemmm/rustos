## NAME

ls — lister le contenu des répertoires

## SYNOPSIS

`ls [-aACcdFfghilmnopQrRsStUuvXx1] [-w cols] [--time=WORD]`
`[--time-style=STYLE] [--sort=WORD] [--full-time]`
`[--group-directories-first] [--] [path...]`

## DESCRIPTION

Liste chaque opérande de chemin : les entrées d'un opérande répertoire
sont lues et listées (sauf si `-d` désigne le répertoire lui-même),
tout autre opérande est listé tel quel. Sans opérande, le répertoire
courant (`.`) est listé.

Les entrées sont triées par nom (ou par taille, la plus grande en
premier, avec `-S` ; inversées avec `-r`), un nom par ligne par
défaut. Les entrées dont le nom commence par `.` sont masquées sauf si
`-a` ou `-A` est donné ; quand des entrées sont masquées, une note est
émise sur le flux d'information standard (fd 3), jamais dans la liste
elle-même.

Le format long (`-l`) affiche le type et les bits de permission, le
propriétaire et le groupe, la taille, puis le nom. Le propriétaire et
le groupe sont des identifiants numériques : résoudre les noms de
comptes exige la base d'utilisateurs protégée par capacité, qu'une
liste ne doit pas exiger ; la sortie correspond donc au repli
numérique de l'outil GNU (`-n` produit la même chose). Il n'y a pas de
colonne d'horodatage affiche l'heure de modification par défaut ;
`-c`, `-u` et `--time` choisissent laquelle des quatre marques est
affichée (et sert au tri), et `--time-style` — ou `--full-time` —
fixe son format. Il n'y a pas encore de colonne de nombre de liens
car le contrat du système de fichiers ne porte pas encore de liens
physiques ; elle apparaîtra quand ce sera le cas.

Quand plusieurs opérandes sont donnés — et toujours sous `-R` — la
liste de chaque répertoire est précédée d'un en-tête `chemin:`, et les
blocs sont séparés par une ligne vide.

## OPTIONS

- `-t` — trier par l'horodatage affiché, le plus récent en premier.
- `-c` — utiliser l'heure de changement de métadonnées (ctime) : avec
  `-l` l'afficher et avec `-t` trier par elle ; sans `-l`, trier par
  elle.
- `-u` — comme `-c`, mais l'heure d'accès (atime).
- `-i, --inode` — afficher le numéro de nœud de chaque entrée.
- `--time=WORD` — quel horodatage afficher et selon lequel trier :
  `atime` (`access`, `use`), `ctime` (`status`), `mtime`
  (`modification`) ou `birth` (`creation`).
- `--time-style=STYLE` — format de l'horodatage : `locale` (par
  défaut), `long-iso`, `full-iso` ou `iso`. Un `+FORMAT` personnalisé
  n'est pas pris en charge.
- `--full-time` — comme `-l --time-style=full-iso`.
- `-a, --all` — ne pas masquer les entrées dont le nom commence par
  `.`.
- `-A, --almost-all` — comme `-a`, mais sans jamais lister `.` ni
  `..`.
- `-d, --directory` — lister les opérandes répertoires eux-mêmes, pas
  leur contenu.
- `-F, --classify` — ajouter `/` aux répertoires et `*` aux
  exécutables.
- `-g` — format long sans la colonne propriétaire ; implique `-l`.
- `-h, --human-readable` — avec `-l`, afficher les tailles comme
  `1.1K`, `23M` (puissances de 1024).
- `-l` — format long : bits de permission, propriétaire, groupe,
  taille, puis nom.
- `-m` — noms séparés par des virgules, répartis sur la largeur.
- `-n, --numeric-uid-gid` — format long avec propriétaire et groupe
  numériques ; implique `-l`. Le propriétaire et le groupe sont
  toujours numériques ici (voir ci-dessus), donc identique à `-l`.
- `-o` — format long sans la colonne groupe ; implique `-l`.
- `-p` — ajouter `/` aux répertoires.
- `-Q, --quote-name` — entourer chaque nom de guillemets doubles, en
  échappant guillemets, barres obliques inverses et caractères de
  contrôle.
- `-r, --reverse` — inverser l'ordre de tri.
- `-R, --recursive` — lister les sous-répertoires récursivement.
- `-s, --size` — afficher la taille allouée de chaque entrée en blocs de
  1024 octets (mise à l'échelle avec `-h`), avec une ligne `total` par
  répertoire listé.
- `-C` — lister en colonnes, remplies de haut en bas (par défaut sur
  un terminal).
- `-S` — trier par taille, la plus grande en premier.
- `-U` — ne pas trier ; lister les entrées dans l'ordre du répertoire.
- `-X` — trier par extension de nom (le texte à partir du dernier
  `.`), à égalité par nom.
- `-v` — tri « version » naturel, de sorte que `f2` précède `f10` ;
  à égalité par nom.
- `-f` — ne pas trier et afficher toutes les entrées : active `-a` et
  `-U` et désactive `-l` et `-s`. Appliqué à sa position, donc un
  `-l`/`-s`/indicateur de tri ultérieur le remplace.
- `--sort=WORD` — choisir la clé de tri par nom : `none` (`-U`),
  `size` (`-S`), `time` (`-t`), `version` (`-v`), `extension` (`-X`)
  ou `name`.
- `--group-directories-first` — lister les répertoires avant les
  autres entrées ; les répertoires en premier même avec `-r`.
- `-w, --width <cols>` — définir la largeur de sortie en colonnes ;
  `0` signifie illimitée.
- `-x` — lister en colonnes, remplies de gauche à droite.
- `-1` — un nom par ligne (le comportement par défaut).
- `-?` — afficher l'aide courte de cette commande (`--help` est la
  forme longue).

## EXAMPLES

- `ls` — lister le répertoire courant.
- `ls -al /System` — liste au format long de `/System`, entrées
  masquées comprises.
- `ls -lhS` — format long, tailles lisibles, la plus grande en
  premier.
- `ls -R Documents` — parcourir `Documents` récursivement, un en-tête
  par répertoire.
- `ls -F` — marquer les répertoires avec `/` et les exécutables avec
  `*`.
- `ls -d Documents` — lister l'entrée `Documents` elle-même, pas son
  contenu.

## EXIT STATUS

- `0` — chaque opérande a été listé.
- `1` — un opérande n'a pas pu être inspecté ou un répertoire n'a pas
  pu être lu, ou la sortie n'a pas pu être délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `cat`
- `man`
