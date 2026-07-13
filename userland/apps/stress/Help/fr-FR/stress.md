## NAME

stress — charger à la demande le CPU, la mémoire, le disque et les caches

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Lance des processus de travail qui chargent délibérément la machine,
dans l'esprit des outils établis `stress`/`stress-ng` : boucles CPU
(`--cpu`), travailleurs mémoire allouer-et-toucher (`--vm`),
écriture/synchronisation de petits tampons (`--io`), écrivains disque
séquentiels volumineux (`--hdd`) et relecteurs brassant les caches
(`--cache`, un ajout RustOS). Chaque travailleur est un processus
propre et paginable ; le processus de contrôle épingle sa propre
mémoire (`mem_pin`, exigeant `CAP_MEM_PIN`) pour rester réactif sous la
pression qu'il crée lui-même, et observe `Ctrl-C`/`Terminate`, de sorte
que chaque fin de l'exécution — achèvement, délai ou signal — arrête
les travailleurs, les recueille et supprime chaque fichier de travail.

Les cibles mémoire et disque sont dimensionnées d'après la machine
elle-même : sauf valeurs explicites via `--vm-bytes`/`--hdd-bytes`, les
travailleurs vm se partagent la moitié de la RAM découverte et les
travailleurs hdd la moitié de l'espace libre du volume de travail.
`--overcommit P` remet ces cibles découvertes à `P` pour cent de la
ressource ; au-delà de 100, les travailleurs poussent dans la pression,
et les refus typés produits (volume plein, limite de ressources) sont
comptés et rapportés comme des résultats attendus — jamais retentés,
jamais un plantage. Charger la machine n'exige aucun privilège au-delà
des propres limites de ressources de l'appelant — les limites sont la
défense, et `stress` les respecte.

Les travailleurs touchant au disque n'écrivent que sous le répertoire
de travail — le répertoire de cache par utilisateur de l'application
(`$HOME/Library/stress`) sauf si `--temp-path` en nomme un autre — et
chaque fichier de travail est supprimé au démontage, y compris sur les
chemins des signaux.

Un résumé est imprimé à la fin de l'exécution (supprimé par
`--quiet`), et un enregistrement `summary` lisible par machine est émis
sur le flux d'information standard consultatif (fd 3).

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — lancer `N` travailleurs
  du genre nommé, avec la signification de GNU `stress`.
- `--cache N` — lancer `N` brasseurs de caches (RustOS uniquement :
  des parcours de répertoires à froid et des relectures répétés font
  bouger les registres de caches récupérables du noyau).
- `--all N` — `N` travailleurs de chaque genre.
- `--vm-bytes B`, `--hdd-bytes B` — la cible en octets de chaque
  travailleur, avec les suffixes GNU (`k`, `m`, `g`, `t` ; p. ex.
  `256M`). Les valeurs par défaut sont dimensionnées d'après la RAM /
  l'espace libre découverts.
- `--overcommit P` — mettre les cibles vm/hdd découvertes à `P` pour
  cent de la ressource ; peut dépasser 100 (les refus sont alors des
  résultats attendus).
- `--timeout T` — s'arrêter après `T` (suffixes `s`/`m`/`h` ; p. ex.
  `5m`). Pas de valeur par défaut : sans lui, l'exécution continue
  jusqu'à ce qu'un signal y mette fin.
- `--temp-path DIR` — le répertoire de travail des travailleurs
  touchant au disque.
- `--monitor` — faire tourner `sysmon` au premier plan pendant la
  durée ; l'exécution est rapportée quand le moniteur se termine.
  Contredit `--background`.
- `-q, --quiet` — supprimer le résumé et les lignes de progression
  sur stdout (les erreurs atteignent toujours stderr).
- `--background` — imprimer le PID du contrôleur détaché et rendre
  l'invite (implique `--quiet`). La forme `&` du shell fonctionne
  aussi ; ce drapeau est pour les scripts.
- `-h, -?, --help` — afficher l'aide courte de cette commande et
  quitter.
- `--version` — imprimer le nom et la version de l'outil et quitter.

## EXIT STATUS

- `0` — l'exécution s'est achevée (les refus typés des travailleurs
  sont des résultats attendus et ne la font pas échouer).
- `1` — un travailleur a réellement échoué, ou l'exécution n'a pas pu
  être mise en place.
- `2` — la ligne de commande n'a pas été comprise.
- `130` / `143` — `Ctrl-C` / `Terminate` a mis fin à l'exécution,
  après le démontage des travailleurs et la suppression des fichiers
  de travail.

## ENVIRONMENT

- `HOME` — localise le répertoire de travail par défaut
  (`$HOME/Library/stress`).
- `LANG` — la locale préférée de l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
