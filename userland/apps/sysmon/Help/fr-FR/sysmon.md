## NAME

sysmon — observer en direct la mémoire et la charge du noyau

## SYNOPSIS

`sysmon [-d sec.dixièmes] [-h | -?]`

## DESCRIPTION

Affiche en plein écran, en direct, la mémoire et la charge du noyau via
l'API d'informations système : mémoire physique, tas du noyau, bande de
pression mémoire avec son historique, registre des caches récupérables,
étage compressé `ramzip`, total de mémoire épinglée, charge par CPU et
recensement des processus. L'outil reste utilisable pendant une charge
délibérée et demeure au repos entre les rafraîchissements.

Au démarrage, le moniteur épingle sa propre mémoire (`mem_pin`, qui
requiert `CAP_MEM_PIN`) afin de ne jamais bloquer sur ses propres
défauts de page sous la pression même qu'il observe. Un épinglage
refusé est signalé sur la ligne de titre et la session continue sans
épinglage — l'épinglage est accessoire, jamais fatal.

L'affichage se rafraîchit à chaque intervalle (3,0 secondes sauf si
`-d` en décide autrement), et `r` le rafraîchit immédiatement. Le
moniteur ne prend aucun opérande : il se pilote au clavier dans la
session.

- `q` — quitter.
- `p` — faire défiler le panneau de détail : caches récupérables,
  étage compressé, charge par CPU, processus.
- `r` — rafraîchir maintenant.
- `+` / `-` — allonger / raccourcir l'intervalle d'une seconde, entre
  0,1 et 60 secondes.
- Haut/Bas, PgPréc/PgSuiv, Début/Fin — faire défiler le panneau.
- `h`, `?` — afficher ou masquer l'aide-mémoire des touches.

Six lignes de synthèse précèdent le panneau de détail : le titre
(durée de fonctionnement, moyennes de charge, état d'épinglage) ; les
chiffres mémoire en MiB avec le total épinglé ; la bande de pression
avec sa jauge, les chiffres libre/réserve et les compteurs d'entrée ;
l'historique des bandes (un glyphe par rafraîchissement : `.` normal,
`-` léger, `=` modéré, `#` sévère, `!` critique) ; la ligne CPU
globale ; et le recensement des tâches.

Chaque chiffre passe par l'API d'informations système — il n'y a pas
de `/proc`. Les requêtes de statistiques du noyau requièrent
`CAP_SYSINFO_KERNEL`, et le recensement de tous les processus
`CAP_SYSINFO_GLOBAL` : sans l'une d'elles, le refus du panneau
concerné est énoncé tandis que le reste de la session continue. La
liste interactive complète des processus relève de `top` ; le panneau
processus ne montre ici que le recensement et les plus gros
consommateurs par `%CPU` et par mémoire.

## OPTIONS

- `-d, --delay <seconds>` — l'intervalle entre les rafraîchissements
  automatiques, en secondes avec fraction facultative (seul le premier
  chiffre décimal, les dixièmes, est conservé) : `sysmon -d 1.5`
  rafraîchit toutes les 1,5 secondes. Par défaut 3,0. GNU `top`
  accepte un intervalle nul et rafraîchit aussi vite que possible ;
  TAIRiX ne boucle jamais à vide, un zéro est donc relevé au minimum
  de 0,1 s.
- `-h, -?` — afficher l'aide courte de cette commande et quitter. Dans
  une session en cours, les mêmes touches basculent l'aide-mémoire.

## EXIT STATUS

- `0` — la session s'est terminée par `q`, ou l'aide courte a été
  affichée.
- `1` — le terminal a échoué ; la raison est écrite sur l'erreur
  standard.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
