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
- `cpuinfo` — le rapport processeur par CPU (un surensemble de
  `/proc/cpuinfo`) : modèle/fabricant, classe de performance, indicateurs
  d'extensions ISA, le registre d'identité brut, la fréquence d'horloge
  de cœur mesurée en direct (en MHz — ou un « unknown » honnête là où
  aucun compteur d'horloge de cœur n'existe) et la fréquence de
  référence/base de temps fixe. Faits matériels publics, aucune
  capacité requise.
- `irq`, `irqs` — la table des IRQ du noyau : une ligne par ligne
  d'interruption associée — son identifiant, la tâche du pilote
  propriétaire, le nombre d'interruptions depuis le démarrage et si la
  ligne est en quarantaine (nécessite `CAP_SYSINFO_HW`).
- `storage`, `io` — la santé des E/S de stockage par volume : une ligne
  par volume sur blocs conscient des pannes — un préfixe de son
  identifiant durable, le point d'accès du service de blocs qui le sert,
  sa disponibilité actuelle (available/degraded/recovering/lost) et les
  compteurs de résultats cumulés (achèvements, réinitialisations,
  expirations, erreurs de support, réémissions) sur lesquels un disque
  défaillant ou instable devient visible (nécessite
  `CAP_SYSINFO_KERNEL`).
- `raid`, `arrays` — les grappes RAID composées et les périphériques que
  détient le compositeur de grappes : une ligne par grappe — un préfixe
  de son identité, son niveau, sa santé
  (optimal/degraded/recovering/failed), le nombre de membres synchronisés
  et définis, son unité d'entrelacement, son nombre de blocs et toute
  reconstruction ou vérification en cours — puis une ligne par
  périphérique — son nœud de l'arbre matériel, la grappe à laquelle il
  appartient (un tiret pour un candidat non affilié), son emplacement,
  son rôle (candidate/held/in-sync/resyncing/faulted), sa taille et la
  génération de métadonnées qu'il porte (nécessite `CAP_SYSINFO_HW`).
- `show <resource-ref>` — lit une référence de ressource
  `info:`/`state:`/`stats:` et affiche sa valeur. Ces espaces de noms
  fournissent des valeurs typées via cette API, jamais des flux d'octets :
  `cat` ne peut pas les ouvrir. Un refus nomme la capacité requise.
- `describe <resource-ref>` — affiche l'enveloppe de la réponse au lieu de
  la valeur : son producteur, l'autorisation sous laquelle elle a été
  servie, et les métadonnées de la charge utile — pour une métrique son
  genre, son unité, son comportement de remise à zéro et sa fenêtre
  d'échantillonnage ; pour un fait son type et sa sensibilité.
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
