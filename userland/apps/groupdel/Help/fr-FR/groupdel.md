## NAME

groupdel — supprimer un groupe

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Retire un groupe du registre des groupes. Supprimer un groupe est une opération d'administration : le registre refuse un appelant dépourvu de la capacité d'administration des utilisateurs.

Le registre fait autorité sur ce qui peut être retiré. Il refuse la suppression d'un groupe qu'un compte référence encore, afin qu'aucun compte ne nomme un groupe inexistant.

`--` termine l'analyse des options : tout argument ultérieur est un opérande.

## OPTIONS

- `-h, -?, --help` — afficher l'aide courte propre à cette commande.

## EXAMPLES

- `groupdel staff` — supprimer le groupe `staff`.

## EXIT STATUS

- `0` — le groupe a été supprimé.
- `1` — le registre a refusé ou échoué la suppression (par exemple une capacité manquante, un groupe inconnu ou un groupe encore référencé) ; la raison est écrite sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle que `fr-FR`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
