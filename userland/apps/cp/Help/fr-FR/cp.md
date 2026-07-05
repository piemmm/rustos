## NAME

cp — copier des fichiers et des répertoires

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Copie chaque opérande source vers une destination. Avec une seule
source et une destination qui ne nomme pas un répertoire, la source
est copiée vers ce chemin exact. Quand la destination nomme un
répertoire existant — et toujours quand il y a plus d'une source —
chaque source est copiée *dans* ce répertoire sous son propre nom de
base.

Une source répertoire n'est copiée qu'avec `-r`, qui reproduit tout le
sous-arbre ; sans `-r` un opérande répertoire est refusé. Un fichier
de destination existant est écrasé par défaut, ignoré avec `-n`, et
fait l'objet d'une question sur la sortie d'erreur avec `-i` (une
question déclinée ignore cette copie sans erreur ; une réponse
illisible ne vaut jamais consentement).

Le premier échec arrête l'exécution avant tout opérande ultérieur.
`--` termine l'analyse des options : chaque argument ultérieur est un
chemin.

## OPTIONS

- `-r, -R, --recursive` — copier les répertoires et leur contenu.
- `-f, --force` — quand un fichier de destination ne peut pas être
  créé, le supprimer et réessayer la copie une fois.
- `-i, --interactive` — demander avant d'écraser un fichier existant ;
  seule une réponse commençant par `y`/`Y` consent.
- `-n, --no-clobber` — ne jamais écraser un fichier existant. Le
  dernier de `-i` et `-n` l'emporte.
- `-v, --verbose` — signaler chaque copie sous la forme
  `'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — copier chaque source dans `dir`,
  qui doit être un répertoire existant. La valeur suit attachée
  (`-tdir`, `--target-directory=dir`) ou comme argument suivant.
- `-T, --no-target-directory` — traiter la destination comme un
  fichier ordinaire ; exactement une source est permise. Incompatible
  avec `-t`.
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `cp notes.txt backup.txt` — copier un fichier sous un nouveau nom.
- `cp -r Projects Archive` — reproduire l'arbre `Projects` dans
  `Archive` (ou comme `Archive` s'il n'existe pas).
- `cp -v -t Backup a.txt b.txt` — copier les deux fichiers dans
  `Backup`, en signalant chaque copie.

## EXIT STATUS

- `0` — chaque copie a réussi (un saut `-n` et une question `-i`
  déclinée ne sont pas des échecs).
- `1` — un échec du système de fichiers, de la question ou de la
  sortie ; la raison est imprimée sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
