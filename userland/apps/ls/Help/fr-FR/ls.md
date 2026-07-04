## NAME

ls — lister le contenu des répertoires

## SYNOPSIS

`ls [-a] [-l] [--] [path...]`

## DESCRIPTION

Liste chaque opérande de chemin dans l'ordre. Pour un répertoire, ses
entrées sont listées, triées par nom ; un opérande qui n'est pas un
répertoire est listé par son nom. Sans opérande, le répertoire courant
est listé.

Les entrées dont le nom commence par `.` sont masquées sauf si `-a` est
donné. Quand le filtre par défaut masque des entrées, `ls` en note le
nombre sur le flux consultatif (fd 3) ; la liste elle-même est
inchangée.

Avec plusieurs opérandes, ceux qui ne sont pas des répertoires sont
listés d'abord (triés par nom), puis chaque répertoire sous un en-tête
`chemin:`, les blocs étant séparés par une ligne vide.

Le format long affiche, par entrée : un caractère de type (`d` pour un
répertoire, `-` sinon), les neuf bits de permission, la taille en
octets alignée à droite sur le bloc, puis le nom.

## OPTIONS

- `-a, --all` — ne pas masquer les entrées dont le nom commence par `.`.
- `-l, --long` — format long : type et bits de permission, taille, puis
  nom.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `ls` — lister le répertoire courant.
- `ls -la /System/Apps` — lister toutes les entrées de `/System/Apps`,
  y compris les masquées, au format long.
- `ls -- -a` — lister le fichier ou répertoire nommé `-a`.

## EXIT STATUS

- `0` — chaque opérande a été listé.
- `1` — un opérande n'a pas pu être inspecté, un répertoire n'a pas pu
  être lu, ou la liste n'a pas pu être délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
