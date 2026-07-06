## NAME

rmdir — supprimer des répertoires vides

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] répertoire...`

## DESCRIPTION

Supprime chaque répertoire donné en opérande, dans l'ordre. Seul un
**répertoire vide** est supprimé : le système de fichiers lui-même
refuse un fichier (ou tout autre objet) et un répertoire non vide, de
manière atomique, si bien que rien d'autre ne peut jamais être supprimé
à sa place. Utilisez `rm` pour les fichiers et `rm -r` pour les
arborescences non vides.

Avec `-p`, les ancêtres de chaque opérande sont supprimés aussi, du
plus interne au plus externe : `rmdir -p a/b/c` supprime `a/b/c`, puis
`a/b`, puis `a`. La racine nue d'un chemin (`/` ou une racine d'alias
telle que `Home:/`) n'est jamais visée.

Avec `--ignore-fail-on-non-empty`, un refus « répertoire non vide »
n'est pas une erreur — l'opérande (ou la remontée de `-p`) s'arrête
simplement là. Aucun autre refus n'est toléré. Le premier échec réel
arrête l'exécution avant tout opérande suivant. `--` termine l'analyse
des options : chaque argument suivant est un chemin.

## OPTIONS

- `-p, --parents` — supprimer aussi les ancêtres de chaque opérande, du
  plus interne au plus externe.
- `-v, --verbose` — signaler chaque tentative de suppression par
  `rmdir: removing directory, 'rép'`.
- `--ignore-fail-on-non-empty` — un répertoire non vide n'est pas une
  erreur ; avec `-p` la remontée s'arrête là.
- `-h, -?` — afficher l'aide courte de cette commande (aussi `--help`).

## EXAMPLES

- `rmdir Scratch` — supprimer un répertoire vide.
- `rmdir -p Projects/os/build` — supprimer la chaîne, du plus interne
  au plus externe.
- `rmdir -p --ignore-fail-on-non-empty a/b` — supprimer `a/b`, et `a`
  aussi si cela le laisse vide.

## EXIT STATUS

- `0` — chaque suppression a réussi (un refus toléré par
  `--ignore-fail-on-non-empty` n'est pas un échec).
- `1` — un échec du système de fichiers ou de la sortie ; la raison est
  affichée sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

mkdir, rm, ls
