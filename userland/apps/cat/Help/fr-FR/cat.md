## NAME

cat — concaténer des fichiers vers la sortie standard

## SYNOPSIS

`cat [-n] [--] [file...]`

## DESCRIPTION

Lit chaque opérande de fichier dans l'ordre et écrit ses octets sur la
sortie standard. L'opérande `-` désigne l'entrée standard, et sans
opérande l'entrée standard est l'unique source.

Avec `-n`, les lignes de sortie sont numérotées en continu sur toutes
les sources, de sorte qu'une ligne à cheval sur deux sources n'est
numérotée qu'une seule fois, à l'apparition de son premier octet.

Une source qui ne peut pas être lue arrête la commande avant qu'une
source ultérieure ne soit touchée ; les octets déjà écrits le restent.

## OPTIONS

- `-n, --number` — numéroter les lignes de sortie, en continu sur
  toutes les sources.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `cat notes.txt` — écrire `notes.txt` sur la sortie standard.
- `cat a.txt - b.txt` — écrire `a.txt`, puis l'entrée standard, puis
  `b.txt`.
- `cat -n log.txt` — numéroter chaque ligne de sortie.
- `cat -- -n` — écrire le fichier nommé `-n`.

## EXIT STATUS

- `0` — chaque source a été écrite.
- `1` — une source n'a pas pu être lue, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `ls`
- `man`
