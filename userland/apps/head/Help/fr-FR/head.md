## NAME

head — afficher le début des fichiers

## SYNOPSIS

`head [option...] [file...]`

## DESCRIPTION

Affiche les 10 premières lignes de chaque `file` sur la sortie
standard. Avec plusieurs `file`, chaque partie est précédée d'un
en-tête `==> file <==`. Sans `file`, ou quand `file` vaut `-`, l'entrée
standard est lue.

`-n` et `-c` changent la quantité affichée : un compte simple affiche
les `num` premières lignes ou premiers octets ; un compte écrit avec un
`-` en tête affiche tout **sauf** les `num` dernières lignes ou
derniers octets. Un compte peut porter un suffixe multiplicateur :
`b` (512), `kB` (1000), `K` (1024), `MB`, `M`, `GB`, `G`, et ainsi de
suite pour `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (une lettre seule
multiplie par des puissances de 1024 ; avec `B` par des puissances de
1000 ; avec `iB` par des puissances de 1024).

La forme historique en premier argument `head -num` (avec les
multiplicateurs `b`/`k`/`m` et les lettres `l`/`q`/`v`/`z` finales
facultatives) est acceptée, comme dans l'outil GNU.

Un fichier illisible est signalé sur la sortie d'erreur et l'exécution
continue avec le fichier suivant.

## OPTIONS

- `-c, --bytes <num>` — afficher les `num` premiers octets de chaque
  fichier ; avec un `-` en tête, tout sauf les `num` derniers octets.
- `-n, --lines <num>` — afficher les `num` premières lignes de chaque
  fichier ; avec un `-` en tête, tout sauf les `num` dernières lignes.
- `-q, --quiet, --silent` — ne jamais afficher les en-têtes
  `==> file <==`.
- `-v, --verbose` — toujours afficher les en-têtes `==> file <==`.
- `-z, --zero-terminated` — les lignes sont délimitées par NUL au lieu
  du saut de ligne.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `head log.txt` — afficher les 10 premières lignes de `log.txt`.
- `head -n 3 a b` — afficher les 3 premières lignes de `a` et de `b`,
  chacune sous son en-tête.
- `head -c 1K image` — afficher les 1024 premiers octets de `image`.
- `head -n -1 notes` — afficher `notes` sans sa dernière ligne.

## EXIT STATUS

- `0` — chaque fichier a été affiché (ou l'aide courte a été écrite).
- `1` — un fichier n'a pas pu être lu, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `cat`
- `wc`
- `man`
