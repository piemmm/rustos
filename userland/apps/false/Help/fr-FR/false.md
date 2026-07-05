## NAME

false — ne rien faire, sans succès

## SYNOPSIS

`false [arguments ignorés]`

## DESCRIPTION

Se termine avec le statut `1`, en ignorant tous les arguments. Les
scripts l'utilisent partout où une commande qui échoue toujours est
nécessaire — condition toujours fausse ou échec délibéré.

Seul un **premier** argument `-h`, `-?` ou `--help` est pris en compte
(la position dans laquelle GNU `false` honore `--help`) ; à toute autre
position, ces mots sont ignorés comme le reste. Contrairement à GNU
`false --help`, qui se termine quand même avec `1`, une aide courte
servie se termine ici avec `0` — la convention d'aide courte de RustOS.

## OPTIONS

- `-h, -?` — (premier argument uniquement) afficher l'aide courte de
  cette commande.

## EXAMPLES

- `false` — échouer.
- `until false; do …; done` — exécuter le corps une fois (la condition
  est toujours fausse).

## EXIT STATUS

- `1` — toujours (c'est la raison d'être de l'outil).
- `0` — l'aide courte demandée a été servie.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `true`
- `man`
