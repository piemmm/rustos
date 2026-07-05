## NAME

ls — lister le contenu des répertoires

## SYNOPSIS

`ls [-aAdFghlmnopQrRS1] [--] [path...]`

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
colonne de nombre de liens ni d'horodatage car le contrat du système
de fichiers ne porte pas encore de liens physiques ni d'horodatages ;
les colonnes apparaîtront quand ce sera le cas.

Quand plusieurs opérandes sont donnés — et toujours sous `-R` — la
liste de chaque répertoire est précédée d'un en-tête `chemin:`, et les
blocs sont séparés par une ligne vide.

## OPTIONS

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
- `-m` — noms séparés par des virgules sur une ligne.
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
- `-S` — trier par taille, la plus grande en premier.
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
