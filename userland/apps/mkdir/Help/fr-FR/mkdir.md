## NAME

mkdir — créer des répertoires

## SYNOPSIS

`mkdir [-pv] [--] répertoire...`

## DESCRIPTION

Crée chaque répertoire donné en opérande, dans l'ordre. Sans `-p`, le
parent de chaque opérande doit déjà exister et l'opérande lui-même ne
doit pas exister ; le premier échec arrête l'exécution avant tout
opérande suivant.

Avec `-p`, chaque ancêtre manquant est créé d'abord, du plus externe au
plus interne, et un opérande (ou un ancêtre) qui existe déjà comme
répertoire n'est pas une erreur. Un ancêtre qui existe comme fichier
échoue toujours : rien n'est jamais remplacé silencieusement.

L'option `-m`/`--mode` de GNU `mkdir` n'est pas encore acceptée : les
répertoires sont créés avec le mode par défaut du système de fichiers
jusqu'à l'arrivée du mécanisme de définition des modes ; l'option
arrivera avec lui plutôt que d'être ignorée. `--` termine l'analyse des
options : chaque argument suivant est un chemin.

## OPTIONS

- `-p, --parents` — créer les répertoires parents manquants ; un
  opérande qui est déjà un répertoire n'est pas une erreur.
- `-v, --verbose` — signaler chaque répertoire créé par
  `mkdir: created directory 'rép'`.
- `-h, -?` — afficher l'aide courte de cette commande (aussi `--help`).

## EXAMPLES

- `mkdir Notes` — créer un répertoire dans le répertoire courant.
- `mkdir -p Projects/os/build` — créer toute la chaîne, en sautant les
  parties qui existent déjà.
- `mkdir -pv Home:/tools/bin` — créer sous une racine d'alias, en
  signalant chaque nouveau répertoire.

## EXIT STATUS

- `0` — chaque répertoire a été créé (ou, avec `-p`, existait déjà).
- `1` — un échec du système de fichiers ou de la sortie ; la raison est
  affichée sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

rmdir, rm, ls
