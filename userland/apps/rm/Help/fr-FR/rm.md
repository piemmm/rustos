## NAME

rm — supprimer des fichiers et des répertoires

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Supprime chaque opérande fichier, dans l'ordre. Un opérande qui n'est
pas un répertoire est délié ; un opérande répertoire n'est supprimé
qu'avec `-r` (qui supprime son contenu en profondeur d'abord, puis le
répertoire lui-même) ou, s'il est vide, avec `-d`.

Avec `-f`, un opérande qui n'existe pas est ignoré silencieusement et
aucune question n'est jamais posée. `-i` demande sur la sortie
d'erreur avant chaque suppression et avant de descendre dans un
répertoire ; `-I` demande une seule fois avant de supprimer plus de
trois opérandes ou avant une suppression récursive. Une question
déclinée ignore l'objet (ou toute l'exécution, pour `-I`) sans
erreur ; une réponse illisible ne vaut jamais consentement. Le
dernier de `-f`, `-i` et `-I` l'emporte.

L'opérande `/` est refusé sous `--preserve-root`, le comportement par
défaut. Le premier échec arrête l'exécution avant tout opérande
ultérieur. `--` termine l'analyse des options : chaque argument
ultérieur est un chemin.

## OPTIONS

- `-r, -R, --recursive` — supprimer les répertoires et leur contenu.
- `-f, --force` — ignorer les opérandes qui n'existent pas ; ne
  jamais demander.
- `-d, --dir` — supprimer les répertoires vides.
- `-i, --interactive` — demander avant chaque suppression ; seule une
  réponse commençant par `y`/`Y` consent.
- `-I` — demander une seule fois avant de supprimer plus de trois
  opérandes, ou avant une suppression récursive.
- `-v, --verbose` — signaler chaque suppression sous la forme
  `removed 'file'`.
- `--preserve-root` — refuser de supprimer `/` (le défaut).
- `--no-preserve-root` — autoriser la suppression de `/`.
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `rm notes.txt` — supprimer un fichier.
- `rm -r Scratch` — supprimer l'arbre `Scratch` et tout son contenu.
- `rm -I a b c d` — demander une fois, puis supprimer les quatre
  fichiers sur un `y`.

## EXIT STATUS

- `0` — chaque suppression a réussi (une question déclinée et un saut
  `-f` ne sont pas des échecs).
- `1` — un échec du système de fichiers, de la question ou de la
  sortie ; la raison est imprimée sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
