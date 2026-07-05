## NAME

true — ne rien faire, avec succès

## SYNOPSIS

`true [arguments ignorés]`

## DESCRIPTION

Se termine avec le statut `0`, en ignorant tous les arguments. Les
scripts l'utilisent partout où une commande qui réussit toujours est
nécessaire — commande de substitution, condition toujours vraie ou corps
d'une boucle.

Seul un **premier** argument `-h`, `-?` ou `--help` est pris en compte
(la position dans laquelle GNU `true` honore `--help`) ; à toute autre
position, ces mots sont ignorés comme le reste.

## OPTIONS

- `-h, -?` — (premier argument uniquement) afficher l'aide courte de
  cette commande.

## EXAMPLES

- `true` — réussir.
- `while true; do …; done` — boucler jusqu'à interruption.

## EXIT STATUS

- `0` — toujours (c'est la raison d'être de l'outil).
- `1` — l'aide courte demandée n'a pas pu être écrite.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `false`
- `man`
