## NAME

tail — afficher la fin des fichiers

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Affiche les 10 dernières lignes de chaque `file` sur la sortie
standard. Avec plusieurs `file`, chaque partie est précédée d'un
en-tête `==> file <==`. Sans `file`, ou quand `file` vaut `-`, l'entrée
standard est lue.

`-n` et `-c` changent la quantité affichée : un compte simple (ou écrit
avec un `-` en tête) affiche les `num` dernières lignes ou derniers
octets ; un compte écrit avec un `+` en tête affiche tout **à partir**
de la ligne ou de l'octet `num` (à compter de 1) jusqu'à la fin. Un
compte peut porter un suffixe multiplicateur : `b` (512), `kB` (1000),
`K` (1024), `MB`, `M`, `GB`, `G`, et ainsi de suite pour `T`, `P`, `E`,
`Z`, `Y`, `R`, `Q` (une lettre seule multiplie par des puissances de
1024 ; avec `B` par des puissances de 1000 ; avec `iB` par des
puissances de 1024).

La forme historique en premier argument `tail -num` / `tail +num` (avec
une lettre finale `b`/`c`/`l` facultative) est acceptée, comme dans
l'outil GNU.

Le mode suivi (`-f`, `-F`, `--follow`, `--retry`, `--pid`,
`--sleep-interval`, `--max-unchanged-stats`) n'est pas encore
disponible et est signalé comme une option inconnue : il nécessite une
source de réveil sur changement de fichier que le système n'expose pas
encore, et aucune attente active n'est fournie à sa place.

Quand du contenu en tête n'est pas affiché, un enregistrement
consultatif est écrit sur le flux d'information standard (fd 3) ; il ne
change jamais la sortie ni le code de sortie. Un fichier illisible est
signalé sur la sortie d'erreur et l'exécution continue avec le fichier
suivant.

## OPTIONS

- `-c, --bytes <num>` — afficher les `num` derniers octets de chaque
  fichier ; avec un `+` en tête, tout à partir de l'octet `num`.
- `-n, --lines <num>` — afficher les `num` dernières lignes de chaque
  fichier ; avec un `+` en tête, tout à partir de la ligne `num`.
- `-q, --quiet, --silent` — ne jamais afficher les en-têtes
  `==> file <==`.
- `-v, --verbose` — toujours afficher les en-têtes `==> file <==`.
- `-z, --zero-terminated` — les lignes sont délimitées par NUL au lieu
  du saut de ligne.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `tail log.txt` — afficher les 10 dernières lignes de `log.txt`.
- `tail -n 3 a b` — afficher les 3 dernières lignes de `a` et de `b`,
  chacune sous son en-tête.
- `tail -c 1K image` — afficher les 1024 derniers octets de `image`.
- `tail -n +5 notes` — afficher `notes` à partir de sa 5e ligne.

## EXIT STATUS

- `0` — chaque fichier a été affiché (ou l'aide courte a été écrite).
- `1` — un fichier n'a pas pu être lu, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
