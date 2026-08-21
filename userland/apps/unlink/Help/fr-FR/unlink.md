## NAME

unlink — supprimer un seul nom

## SYNOPSIS

`unlink [--] fichier`

## DESCRIPTION

Supprime exactement un nom, par le seul appel du système de fichiers que
nomme la fonction POSIX `unlink`. Il n'y a volontairement ni récursion,
ni forçage, ni confirmation, ni compte rendu : un script qui doit
supprimer un seul nom et rien d'autre dispose d'un outil incapable d'en
faire plus. Utilisez `rm` pour ces options et `rmdir` pour un répertoire.

Le nom est supprimé **tel qu'il est écrit**. Un lien symbolique est
supprimé lui-même et n'est jamais suivi : un lien placé à cet endroit ne
peut donc pas rediriger la suppression vers sa cible.

Un **répertoire** est refusé par le système de fichiers, dans le même
parcours verrouillé qui aurait supprimé l'entrée — aucune course entre la
vérification et la suppression n'existe ici.

Exactement un opérande est requis : aucun opérande comme deux opérandes
ou plus sont des erreurs d'usage, et rien n'est supprimé. `--` termine
l'analyse des options, de sorte qu'un nom commençant par un tiret reste
supprimable.

## OPTIONS

- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `unlink obsolete.log` — supprimer un nom.
- `unlink Home:/Documents/alias` — supprimer le lien symbolique
  lui-même, non ce qu'il désigne.
- `unlink -- -nom-etrange` — supprimer un nom commençant par un tiret.

## EXIT STATUS

- `0` — le nom a été supprimé (ou l'aide courte a été écrite).
- `1` — le système de fichiers a refusé la suppression, ou la sortie a
  échoué ; la raison est affichée sur la sortie d'erreur standard.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
