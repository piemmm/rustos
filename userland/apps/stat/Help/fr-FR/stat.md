## NAME

stat — afficher l'état d'un fichier ou d'un système de fichiers

## SYNOPSIS

`stat [-Lft] [-c FORMAT | --printf=FORMAT] [--] fichier...`

## DESCRIPTION

Affiche les champs d'un état lu par opérande, dans l'ordre de la ligne de
commande.

**Sans `-L` un lien symbolique est décrit tel qu'il est** — c'est à cela
que sert cet outil à côté de `ls`. `%N` montre le lien et la cible qu'il
stocke, `%F` dit `symbolic link`, et les tailles et horodatages sont
ceux du lien. `-L` résout le dernier lien et décrit ce qu'il désigne.

`-f` passe au système de fichiers qui porte l'opérande : les nombres de
blocs et d'inodes du volume, sa taille de bloc, et le type que son
montage enregistre. Les deux lectures ont des vocabulaires de champs
**différents**, donc un format est vérifié contre celui que `-f`
sélectionne.

`-c`/`--format` affiche une chaîne de format par opérande, suivie d'un
saut de ligne ; `--printf` interprète les échappements et n'ajoute aucun
saut. C'est la seule différence. Une directive accepte les drapeaux et la
largeur de printf (`%-10s`, `%06i`, `%.3n`), afin qu'un rapport tienne en
colonnes. `-t` est la forme concise d'une ligne, pour l'une ou l'autre
lecture.

Un opérande illisible est signalé sur la sortie d'erreur standard, les
opérandes restants sont tout de même décrits, et la commande termine avec
un état non nul. Un champ que ce système ne peut fournir — un instantané
des montages qu'il n'a pas le droit de lire, un uid sans nom dans
l'annuaire des utilisateurs — s'affiche `?` ou `UNKNOWN`, jamais comme un
substitut plausible.

Au moins un opérande est requis. `--` termine l'analyse des options.

Quatre champs nomment une notion que TAIRiX n'a pas, et sont **refusés**
nommément quand un format en emploie un, plutôt que remplis d'une valeur
inventée : `%G`, car l'API d'information système publie un annuaire des
utilisateurs et aucun pendant pour les groupes, de sorte que `%g`
(l'identifiant numérique) est le champ honnête ; `%t` et `%T` du
vocabulaire fichier, car il n'existe aucun fichier spécial de
périphérique dont on aurait un type majeur ou mineur ; et `%t` du
vocabulaire système de fichiers, car un volume n'a pas de nombre magique
de type — `%T` nomme le type que son montage enregistre. Le refus a lieu
à l'analyse du format, avant qu'aucun chemin ne soit touché.

Deux champs rapportent une notion TAIRiX au lieu d'une notion Linux. Un
volume est identifié par un identifiant de 16 octets et non par un numéro
de périphérique, donc `%d` est cet identifiant en décimal et `%D` en
hexadécimal ; comparer les `%d` de deux fichiers répond toujours
exactement à « sont-ils sur un même volume ? ».

## OPTIONS

- `-L, --dereference` — décrire ce que désigne un lien symbolique, plutôt
  que le lien lui-même.
- `-f, --file-system` — décrire le système de fichiers qui porte chaque
  opérande plutôt que l'opérande.
- `-c, --format=FORMAT` — afficher `FORMAT` pour chaque opérande, suivi
  d'un saut de ligne.
- `--printf=FORMAT` — comme `-c`, mais interpréter les échappements et
  n'afficher aucun saut de ligne final.
- `-t, --terse` — afficher les champs sur une seule ligne, séparés par
  des espaces.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `stat notes.txt` — le rapport complet d'un fichier.
- `stat -c '%s %n' *` — taille et nom, une ligne par fichier.
- `stat -L lien` — décrire ce que le lien désigne, non le lien.
- `stat -f .` — le volume portant le répertoire de travail.

## EXIT STATUS

- `0` — chaque opérande a été décrit (ou l'aide courte a été écrite).
- `1` — au moins un opérande était illisible, ou la sortie a échoué.
- `2` — la ligne de commande n'a pas été comprise, ou son format nommait
  une directive que ce système ne peut servir.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

ls, readlink, df, du
