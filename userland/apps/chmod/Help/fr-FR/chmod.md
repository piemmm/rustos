## NAME

chmod — modifier les bits de mode d'un fichier

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Change les bits de permission de chaque opérande fichier en `MODE`,
dans l'ordre. `MODE` est soit une valeur octale absolue (`644`,
`0755`, …) qui remplace entièrement les bits de permission, soit une
liste de clauses symboliques séparées par des virgules
`[ugoa]*[-+=][rwxXst]*` (`g+w`, `o-rx`, `a=rx`, `u+s`) qui
transforment les bits actuels du fichier. Le `X` symbolique n'accorde
l'exécution qu'à un répertoire ou à un fichier portant déjà un bit
d'exécution.

Seul le propriétaire d'un fichier peut changer son mode ; le noyau
refuse quiconque d'autre, et détenir une capability n'accorde aucun
passe-droit. Avec `-R`, un opérande répertoire est modifié puis son
contenu l'est récursivement. Le premier échec arrête l'exécution avant
tout opérande ultérieur. `--` termine l'analyse des options : chaque
argument ultérieur est un opérande. Pour un mode commençant par `-`,
écrivez-le sans le tiret (`a-w`) ou terminez d'abord les options
(`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — modifier fichiers et répertoires
  récursivement.
- `-c, --changes` — ne signaler que les fichiers dont le mode a
  réellement changé.
- `-v, --verbose` — signaler chaque fichier traité.
- `-f, --silent, --quiet` — supprimer la plupart des messages
  d'erreur ; l'exécution échoue quand même et le code de sortie le
  signale.
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `chmod 644 notes.txt` — lecture/écriture pour le propriétaire,
  lecture seule pour les autres.
- `chmod g+w shared.txt` — ajouter l'écriture du groupe aux bits
  actuels.
- `chmod -R a=rx Docs` — rendre l'arbre `Docs` lisible et
  traversable par tous.

## EXIT STATUS

- `0` — chaque changement de mode a réussi.
- `1` — un échec du système de fichiers ou de la sortie ; la raison
  est imprimée sur la sortie d'erreur (supprimée sous `-f`).
- `2` — la ligne de commande n'a pas été comprise, ou l'opérande de
  mode n'était ni octal ni symbolique.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
