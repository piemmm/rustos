## NAME

users — administrer les comptes utilisateur et les groupes

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Lance la session interactive d'administration des comptes via
l'interface contrôlée `users_admin`. Chaque opération est décidée côté
noyau selon votre identité attestée par le noyau : sans
`CAP_USER_ADMIN` dans le plafond de votre compte, toute opération est
refusée à l'aiguillage. Les mots de passe sont lus avec l'écho du
terminal désactivé et hachés côté client dans un enregistrement salé ;
le texte en clair ne traverse jamais l'interface et n'est jamais
affiché ni journalisé.

L'outil ne prend aucun opérande : les comptes s'administrent avec des
commandes tapées dans la session.

- `list` — lister les comptes utilisateur.
- `groups` — lister les groupes.
- `create <name> <uid> <gid>` — créer un compte.
- `passwd <name>` — définir le mot de passe d'un compte.
- `lock <name>`, `unlock <name>` — désactiver ou réactiver un compte.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — modifier les
  capacités accordées à un compte.
- `deluser <name>` — supprimer un compte.
- `addgroup`, `delgroup` — créer ou supprimer un groupe.
- `help` — lister les commandes de la session.
- `exit`, `quit` — terminer la session.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande et quitter.

## EXIT STATUS

- `0` — la session s'est terminée proprement, ou l'aide courte a été
  affichée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
