## NAME

sysinfo — interroger les informations système

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Émet une requête typée vers l'API d'information système et affiche la
réponse. TAIRiX n'a ni `/proc` ni `/sys` : cette commande est le visage
terminal de la même API versionnée et contrôlée par capacités que tout
programme utilise, et aucun chemin ne contourne le contrôle de
capacité.

Les requêtes :

- `processes`, `ps` — lister les processus, une ligne par processus.
- `memory`, `mem` — statistiques mémoire du noyau (nécessite
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — l'arbre matériel détecté (nécessite
  `CAP_SYSINFO_HW`).
- `identity`, `id` — identité de la machine et version de l'OS.
- `uptime` — temps écoulé depuis le démarrage et heure du démarrage.
- `limits`, `rlimits` — vos limites de ressources effectives et leur
  usage en direct.
- `seats` — l'inventaire des sièges : le propriétaire de chaque écran
  et sa console de premier plan (nécessite `CAP_SYSINFO_HW`).
- `pressure` — la jauge de pression mémoire en direct : bande, seuils
  et compteurs de transition (nécessite `CAP_SYSINFO_KERNEL`).
- `reclaim` — le registre des caches récupérables, une ligne par classe
  (nécessite `CAP_SYSINFO_KERNEL`).
- `ramzip` — les compteurs du niveau de mémoire compressée (nécessite
  `CAP_SYSINFO_KERNEL`).
- `cpu` — profondeur de file, changements de contexte et préemptions
  par CPU (nécessite `CAP_SYSINFO_KERNEL`).
- `help` — l'aide courte de cette commande.

Sans requête, l'aide courte est affichée.

## OPTIONS

- `--all, -a` — avec `processes` : lister tous les processus du système
  plutôt que seulement les vôtres ; le service n'accorde cette vue qu'à
  un appelant détenant `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `sysinfo identity` — afficher l'identité de la machine et la version
  de l'OS.
- `sysinfo ps --all` — lister tous les processus du système.

## EXIT STATUS

- `0` — la requête a été répondue et affichée.
- `1` — le service a refusé ou échoué, ou le résultat n'a pas pu être
  délivré.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
- `ps`
- `top`
