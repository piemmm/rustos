## NAME

flock — exécuter une commande en détenant un verrou de fichier consultatif

## SYNOPSIS

`flock [options] fichier commande [argument...]`

## DESCRIPTION

Prend un verrou consultatif couvrant tout le `fichier`, exécute la
`commande` pendant qu'il le détient, puis rend le propre code de sortie de
la commande. Deux exécutions nommant le même `fichier` ne se chevauchent
donc jamais, ce dont un script a besoin pour n'avoir qu'une seule copie de
lui-même en cours.

Le verrou appartient au fichier ouvert par cette exécution : le système le
libère lorsque l'exécution se termine, qu'elle aboutisse, soit interrompue
ou plante. Rien n'est écrit dans le `fichier` et rien n'est à nettoyer
ensuite : aucun verrou périmé n'attend la prochaine exécution. Le
`fichier` est créé s'il n'existe pas.

Les verrous sont **consultatifs** : ils coordonnent des programmes qui
acceptent de s'en servir. Ils n'accordent ni ne retirent aucun accès, si
bien qu'un programme qui ne prend pas de verrou n'est pas empêché de lire
ou d'écrire. Qui peut lire ou écrire reste décidé par le propriétaire, les
permissions et la liste d'accès du fichier.

Sans `-n` ni `-w`, l'exécution attend aussi longtemps qu'il le faut. Avec
`-n` elle renonce aussitôt ; avec `-w` après le délai indiqué. Dans les
deux cas, renoncer sort avec le code de conflit (`1` sauf indication de
`-E`) et la commande n'est **pas** exécutée : un script distingue donc
toujours « je n'ai pas obtenu le verrou » de « la commande a échoué ».

Trois options des `flock` d'autres systèmes sont volontairement absentes
plutôt qu'acceptées et ignorées. `-u` et `-o` agissent sur un descripteur
de fichier ouvert au préalable par un interpréteur, forme que cette
commande ne propose pas ; `-c` passe son argument à un interpréteur, ce qui
s'écrit ici explicitement `flock fichier elsh -c '...'` afin que l'on sache
lequel s'exécute. Demander l'une d'elles est une erreur d'usage : le script
est averti au lieu de tourner sans le verrou qu'il demandait.

## OPTIONS

- `-s, --shared` — prendre un verrou partagé. Plusieurs exécutions peuvent en détenir un à la fois, et toutes excluent une demande de verrou exclusif. C'est le verrou des lecteurs.
- `-x, --exclusive` — prendre un verrou exclusif, excluant tout autre détenteur. Valeur par défaut, et verrou des écrivains.
- `-n, --nonblock` — ne pas attendre : si le verrou est détenu, sortir immédiatement avec le code de conflit.
- `-w, --timeout <seconds>` — attendre au plus ce nombre de secondes entières, puis sortir avec le code de conflit.
- `-E, --conflict-code <n>` — code de sortie lorsque `-n` ou `-w` renonce. Vaut `1` par défaut. Choisissez une valeur que la commande ne renvoie jamais si un script doit les distinguer.
- `-v, --verbose` — signaler sur la sortie d'erreur si le verrou a été pris.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — lancer la
  sauvegarde, en attendant si une autre copie tourne déjà.
- `flock -n /Storage/db/data.lock compact` — compacter la base, ou sortir
  aussitôt avec `1` si le verrou est détenu.
- `flock -s -w 30 /Storage/db/data.lock report` — prendre un verrou de
  lecteur, en attendant jusqu'à trente secondes la fin d'un écrivain.
- `flock -E 99 -n lock task` — sortir avec `99` plutôt que `1` quand le
  verrou est détenu, pour distinguer ce cas d'un échec de `task`.

## EXIT STATUS

- le code de la commande — le verrou a été pris et la commande a tourné.
- le code de conflit (`1` par défaut) — `-n` ou `-w` a renoncé ; la
  commande n'a pas tourné.
- `1` — le verrou n'a pu être pris pour une raison qu'attendre ne
  corrigerait pas, ou la commande n'a pu être lancée ; la raison est
  affichée sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise ; rien n'a été verrouillé
  ni exécuté.
- `126` — la commande a été trouvée mais n'a pu être exécutée.
- `127` — la commande n'a pas été trouvée.

## ENVIRONMENT

- `PATH` — parcouru pour la commande, après les magasins de programmes du
  système et de l'utilisateur.
- `HOME` — localise les magasins de programmes de l'utilisateur.
- `LANG` — locale préférée pour l'aide courte (une étiquette BCP-47 telle
  que `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
