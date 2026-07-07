## NAME

whoami — afficher le nom de compte de l'utilisateur courant

## SYNOPSIS

`whoami`

## DESCRIPTION

Affiche le nom d'utilisateur associé à l'identité de ce processus,
suivi d'un saut de ligne, et rien d'autre.

RustOS n'a pas de `/etc/passwd` : l'identifiant d'utilisateur provient
de l'enregistrement que le noyau tient du processus appelant, et le nom
de compte correspondant provient de l'annuaire public des comptes de
l'API d'information système. Si l'annuaire ne contient aucun nom pour
cet identifiant, la commande signale
`cannot find name for user ID <uid>` et échoue.

La commande ne prend aucun opérande ; un argument est une erreur
`extra operand`.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.
- `--` — terminer l'analyse des options ; tout argument ultérieur reste
  un opérande de trop (`whoami` n'en prend aucun).

## EXAMPLES

- `whoami` — afficher le nom du compte qui exécute la commande.

## EXIT STATUS

- `0` — le nom (ou l'aide courte demandée) a été écrit.
- `1` — la lecture de l'identité, la consultation de l'annuaire ou la
  sortie a échoué, ou l'annuaire ne contient aucun nom pour
  l'identifiant.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `users`
- `ps`
