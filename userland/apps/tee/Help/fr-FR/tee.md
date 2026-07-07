## NAME

tee — lire l'entrée standard et écrire sur la sortie standard et dans des fichiers

## SYNOPSIS

`tee [option...] [fichier...]`

## DESCRIPTION

Copie l'entrée standard vers la sortie standard et vers chaque fichier
nommé, afin de voir et de capturer en même temps les données d'un
pipeline. Chaque fichier est créé s'il n'existe pas et écrasé, sauf si
`-a` demande l'ajout en fin de fichier. Un fichier impossible à ouvrir
ou à écrire est signalé et l'exécution continue avec les sorties
restantes, selon le mode `--output-error` choisi.

RustOS n'a pas de `SIGPIPE` : la disparition d'un consommateur se
manifeste par une erreur d'écriture sur la sortie standard — la seule
sortie de cette commande pouvant être un tube — le « tube » des modes
GNU désigne donc exactement cette sortie ici. Sans `--output-error`,
une sortie standard défaillante arrête l'exécution (l'équivalent de
l'outil GNU tué par `SIGPIPE`, la raison étant indiquée sur l'erreur
standard) ; avec un mode `-nopipe`, elle est tolérée en silence.

GNU `tee -i` (ignorer les interruptions) n'est pas disponible : RustOS
n'a pas de disposition de signal par processus à régler. Ce commutateur
arrivera avec ce travail noyau plutôt que d'être accepté et ignoré.

## OPTIONS

- `-a, --append` — ajouter à la fin des fichiers nommés ; ne pas les
  écraser.
- `-p` — tolérer en silence une sortie standard défaillante ;
  équivalent à `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — traitement d'une sortie défaillante. Sans
  valeur, `warn-nopipe`. Les modes (un préfixe non ambigu est
  accepté) : `warn` — signaler une erreur d'écriture sur toute sortie,
  abandonner cette sortie et continuer ; `warn-nopipe` — comme `warn`,
  mais une sortie standard défaillante est abandonnée en silence et ne
  change pas le code de sortie ; `exit` — signaler une erreur
  d'écriture sur toute sortie et s'arrêter ; `exit-nopipe` — comme
  `exit`, mais une sortie standard défaillante est abandonnée en
  silence.
- `-h, -?` — afficher l'aide courte de cette commande.
- `--` — terminer l'analyse des options ; chaque argument suivant nomme
  un fichier, et un opérande `-` nomme un fichier appelé `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — afficher le listage et en garder une
  copie.
- `make 2>&1 | tee -a build.log` — ajouter la transcription d'une
  compilation tout en la regardant.
- `cat data | tee copy1 copy2 | wc -c` — capturer deux copies et
  compter les octets qui continuent.

## EXIT STATUS

- `0` — chaque sortie a été servie jusqu'à la fin de l'entrée (ou
  l'aide courte demandée a été affichée) ; une défaillance de la sortie
  standard tolérée par un mode `-nopipe` ne change rien.
- `1` — une sortie a échoué d'une manière que le mode choisi compte, ou
  l'entrée n'a pas pu être lue.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `cat`
- `head`
- `wc`
