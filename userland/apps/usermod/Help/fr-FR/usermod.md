## NAME

usermod — modifier un compte d'utilisateur

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Modifie les champs d'identité d'un compte, son état de verrouillage ou son
plafond de capacités. Modifier un compte est une opération d'administration
: la base refuse un appelant dépourvu de la capacité d'administration des
utilisateurs.

Une modification d'identité remplace l'ensemble des champs non liés à la
sécurité, aussi l'outil lit d'abord l'enregistrement actuel et renvoie
inchangé chaque champ que personne n'a nommé. Un compte que la base ne liste
pas est refusé avant tout envoi.

Chaque commutateur est une opération distincte de la base, appliquée une à
la fois et entièrement ou pas du tout. Une ligne qui en demande plusieurs en
émet plusieurs, dans un ordre fixe — champs, puis capacités, puis
verrouillage — et s'arrête au premier refus, en nommant l'étape et en
avertissant qu'une modification antérieure peut déjà être en vigueur.

`-G` remplace tout l'ensemble supplémentaire au lieu d'y ajouter : la base
prend des ensembles entiers, et un ajout fondé sur une lecture périmée
serait pire qu'un remplacement explicite. `--grants` est une notion propre à
TAIRiX, écrite en forme longue uniquement ; la base refuse toute capacité
que le compte appelant ne détient pas lui-même.

`--` termine l'analyse des options : tout argument ultérieur est un opérande.

## OPTIONS

- `-c, --comment COMMENT` — le commentaire / nom complet du compte.
- `-d, --home HOME` — le répertoire personnel.
- `-s, --shell SHELL` — l'interpréteur de connexion.
- `-g, --gid GID` — l'identifiant numérique du groupe principal.
- `-G, --groups LIST` — les identifiants numériques des groupes
  supplémentaires, séparés par des virgules, remplaçant l'ensemble actuel.
  Une liste vide l'efface.
- `-L, --lock` — interdire la connexion au compte.
- `-U, --unlock` — l'autoriser de nouveau.
- `--grants LIST` — les noms de capacités, séparés par des virgules, formant
  tout le plafond du compte. Une liste vide l'efface.
- `-h, -?, --help` — afficher l'aide courte propre à cette commande.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — définir le nom complet du compte.
- `usermod -L ada` — verrouiller le compte.
- `usermod --grants LIST` — remplacer le plafond de capacités.

## EXIT STATUS

- `0` — toutes les modifications demandées ont été faites.
- `1` — la base a refusé ou échoué une modification ; l'étape et la raison
  sont écrites sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle que `fr-FR`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
