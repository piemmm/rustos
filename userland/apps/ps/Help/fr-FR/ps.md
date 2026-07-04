## NAME

ps — lister les processus

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Liste les processus via l'API d'information système. Par défaut, seuls
les processus de l'appelant sont listés ; le service applique chaque
portée de requête selon l'identité attestée par le noyau de l'appelant,
et aucun chemin ne contourne ce contrôle.

Chaque processus est affiché sur une ligne sous un en-tête de
colonnes : l'identifiant du processus (`PID`), celui du parent
(`PPID`), les identifiants d'utilisateur et de groupe propriétaires
(`UID`, `GID`), l'état d'ordonnancement (`S`), le CPU sur lequel le
processus a tourné en dernier (`CPU`), et le nom de la commande
(`NAME`).

`ps` ne prend aucun opérande.

## OPTIONS

- `-e, -A, --all` — lister tous les processus du système plutôt que
  seulement ceux de l'appelant ; le service n'accorde cette vue qu'à un
  appelant détenant `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `ps` — lister vos propres processus.
- `ps -e` — lister tous les processus du système.

## EXIT STATUS

- `0` — la liste a été écrite.
- `1` — le service a refusé ou échoué, ou la liste n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
