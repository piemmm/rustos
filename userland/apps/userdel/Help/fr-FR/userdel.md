## NAME

userdel — supprimer un compte d'utilisateur

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Retire un compte de la base des utilisateurs. Supprimer un compte est une opération d'administration : la base refuse un appelant dépourvu de la capacité d'administration des utilisateurs.

La base fait autorité sur ce qui peut être retiré. Elle refuse la suppression du dernier compte actif habilité à administrer les utilisateurs, afin qu'un système ne puisse jamais se retrouver sans moyen d'être administré.

`--` termine l'analyse des options : tout argument ultérieur est un opérande.

## OPTIONS

- `-h, -?, --help` — afficher l'aide courte propre à cette commande.

## EXAMPLES

- `userdel ada` — supprimer le compte `ada`.

## EXIT STATUS

- `0` — le compte a été supprimé.
- `1` — la base a refusé ou échoué la suppression (par exemple une capacité manquante, un compte inconnu ou le dernier administrateur) ; la raison est écrite sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle que `fr-FR`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
