## NAME

useradd — créer un compte utilisateur

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Ajoute un compte unique à la base des utilisateurs. Le nom de connexion
doit correspondre à `[a-z_][a-z0-9_-]*` ; le groupe principal (`-g`) est
obligatoire et chaque référence de groupe ou d'utilisateur est un
identifiant décimal. La création d'un compte est une opération
d'administration : la base refuse un appelant sans la capacité
d'administration des utilisateurs.

Le compte créé n'a **aucun mot de passe utilisable** : aucun mot de passe
ne lui correspond tant qu'un administrateur n'en définit pas un (et aucun
ne peut être deviné), exactement comme l'outil GNU crée un compte
désactivé. Définissez ensuite un mot de passe avec la commande `passwd`
de l'outil `users`.

Quand `-u` est omis, l'identifiant est alloué automatiquement, un
au-dessus du plus haut identifiant existant. Quand `-d` est omis, le
répertoire personnel suit la disposition standard `/Users/NAME`. Le
compte démarre l'interpréteur par défaut du système et le plafond de
capacités de session ordinaire ; un administrateur l'élargit ensuite avec
la commande `grant` de l'outil `users`.

`--` termine l'analyse des options : chaque argument ultérieur est un
opérande.

## OPTIONS

- `-u, --uid UID` — identifiant numérique d'utilisateur ; alloué
  automatiquement quand il est omis (un au-dessus du plus haut existant).
- `-g, --gid GID` — identifiant numérique du groupe principal.
  Obligatoire : il n'y a pas de politique de groupe par défaut à deviner.
- `-G, --groups LIST` — identifiants numériques des groupes
  supplémentaires, séparés par des virgules.
- `-c, --comment TEXT` — commentaire du compte / nom complet affiché.
- `-d, --home PATH` — répertoire personnel ; `/Users/NAME` quand il est
  omis.
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `useradd -g 100 alice` — créer `alice` dans le groupe principal `100`
  avec un identifiant alloué automatiquement.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — chaque champ
  précisé.

## EXIT STATUS

- `0` — le compte a été créé.
- `1` — la base a refusé ou échoué la création (par exemple une capacité
  manquante, un identifiant en double ou un groupe inconnu) ; la raison
  est imprimée sur l'erreur standard.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `groupadd`
- `users`
