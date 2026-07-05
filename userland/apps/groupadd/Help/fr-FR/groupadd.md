## NAME

groupadd — créer un groupe

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

Ajoute un groupe unique au registre des groupes. Le nom du groupe doit
correspondre à `[a-z_][a-z0-9_-]*` et l'identifiant est une valeur
décimale. La création d'un groupe est une opération d'administration :
le registre refuse un appelant sans la capacité d'administration des
utilisateurs.

Quand `-g` est omis, l'identifiant du groupe est alloué automatiquement,
un au-dessus du plus haut identifiant existant. Un identifiant demandé
déjà pris est refusé ; le registre est l'autorité sur les collisions.

`--` termine l'analyse des options : chaque argument ultérieur est un
opérande.

## OPTIONS

- `-g, --gid GID` — identifiant numérique du groupe ; alloué
  automatiquement quand il est omis (un au-dessus du plus haut
  existant).
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `groupadd staff` — créer `staff` avec un identifiant alloué
  automatiquement.
- `groupadd -g 100 staff` — créer `staff` avec l'identifiant `100`.

## EXIT STATUS

- `0` — le groupe a été créé.
- `1` — le registre a refusé ou échoué la création (par exemple une
  capacité manquante ou un identifiant en double) ; la raison est
  imprimée sur l'erreur standard.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `useradd`
- `users`
