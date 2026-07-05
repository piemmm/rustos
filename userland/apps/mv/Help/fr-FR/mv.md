## NAME

mv — déplacer (renommer) des fichiers et des répertoires

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Déplace chaque opérande source vers une destination. Avec une seule
source et une destination qui ne nomme pas un répertoire, la source
est renommée vers ce chemin exact. Quand la destination nomme un
répertoire existant — et toujours quand il y a plus d'une source —
chaque source est déplacée *dans* ce répertoire sous son propre nom
de base.

Un déplacement au sein d'un même volume est un renommage atomique qui
préserve l'identité du nœud. Un déplacement dont la source et la
destination se trouvent sur des volumes différents ne peut pas être
atomique : il se rabat sur une copie de la source vers la
destination, suivie de la suppression de la source (les répertoires
sont reproduits récursivement).

Une destination existante est écrasée par défaut, ignorée avec `-n`,
et fait l'objet d'une question sur la sortie d'erreur avec `-i` (une
question déclinée ignore ce déplacement sans erreur ; une réponse
illisible ne vaut jamais consentement). Le premier échec arrête
l'exécution avant tout opérande ultérieur. `--` termine l'analyse des
options : chaque argument ultérieur est un chemin.

## OPTIONS

- `-f, --force` — supprimer une destination bloquante et réessayer le
  renommage ; ne jamais demander. Le dernier de `-f`, `-i` et `-n`
  l'emporte.
- `-i, --interactive` — demander avant d'écraser une destination
  existante ; seule une réponse commençant par `y`/`Y` consent.
- `-n, --no-clobber` — ne jamais écraser une destination existante.
- `-v, --verbose` — signaler chaque déplacement sous la forme
  `renamed 'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — déplacer chaque source dans
  `dir`, qui doit être un répertoire existant. La valeur suit
  attachée (`-tdir`, `--target-directory=dir`) ou comme argument
  suivant.
- `-T, --no-target-directory` — traiter la destination comme un
  fichier ordinaire ; exactement une source est permise. Incompatible
  avec `-t`.
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `mv draft.txt final.txt` — renommer un fichier.
- `mv -v a.txt b.txt Archive` — déplacer les deux fichiers dans
  `Archive`, en signalant chaque déplacement.
- `mv -n new.cfg current.cfg` — installer un fichier seulement si la
  destination n'existe pas déjà.

## EXIT STATUS

- `0` — chaque déplacement a réussi (un saut `-n` et une question
  `-i` déclinée ne sont pas des échecs).
- `1` — un échec du système de fichiers, de la question ou de la
  sortie ; la raison est imprimée sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
