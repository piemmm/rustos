## NAME

ln — créer des liens symboliques

## SYNOPSIS

`ln -s [-finvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Crée un lien symbolique nommant chaque cible. Avec un seul opérande le
lien est créé dans le répertoire courant sous le nom de la cible. Avec
deux, le second opérande est un répertoire à remplir s'il en est un —
ou un lien vers un répertoire, sauf avec `-n` — et le nom du lien
sinon. Avec trois ou plus, le dernier doit déjà être un répertoire.

La cible est stockée **telle quelle** et n'est jamais résolue : elle
peut être relative, contenir `..`, et ne nommer rien du tout, si bien
qu'un lien peut légitimement pendre. Sa grammaire est tout de même
vérifiée avant l'écriture, donc une cible qu'aucun résolveur ne pourrait
parcourir est refusée. Créer un lien ne donne aucun droit sur ce qu'il
nomme — chaque usage ultérieur est autorisé composant par composant
sous votre propre identité.

Un nom de lien déjà pris est refusé sauf si `-f` ou `-i` demande de le
remplacer, et le remplacement **retire** d'abord ce nom, de sorte que
rien ne passe à travers un lien déjà présent vers ce qu'il désigne. Un
répertoire n'est jamais remplacé.

Le premier échec arrête l'exécution avant toute cible suivante ; les
liens déjà créés subsistent. `--` termine l'analyse des options : tout
argument ultérieur est un opérande.

`-s` est obligatoire sur ce système, qui n'a pas de liens physiques :
sans lui `ln` n'a rien à créer, et il le dit plutôt que de créer un
lien symbolique, qui est un objet différent. Les options réservées aux
liens physiques `-L`, `-P`, `-d` et `-F` sont refusées pour la même
raison. `-b`/`-S` sont refusées car il n'existe aucun mécanisme de
sauvegarde à invoquer, et `-r` car calculer une cible relative au
répertoire du lien exige une résolution canonisante que ce système
n'offre pas — une résolution lexicale nommerait un autre objet dès
qu'un lien serait impliqué.

## OPTIONS

- `-s, --symbolic` — créer des liens symboliques. Obligatoire : voir
  ci-dessus.
- `-f, --force` — retirer un nom de lien existant, puis créer le lien.
- `-i, --interactive` — demander avant de retirer un nom de lien
  existant ; seule une réponse commençant par `y`/`Y` consent. La
  dernière de `-f` et `-i` l'emporte.
- `-n, --no-dereference` — traiter une destination qui est un lien
  symbolique vers un répertoire comme le simple nom qu'elle est aussi,
  plutôt que comme un répertoire où créer les liens.
- `-v, --verbose` — signaler chaque lien créé sous la forme
  `'link' -> 'target'`.
- `-t dir, --target-directory=dir` — créer chaque lien dans `dir`, qui
  doit déjà être un répertoire. La valeur suit attachée (`-tdir`,
  `--target-directory=dir`) ou comme argument suivant.
- `-T, --no-target-directory` — traiter la destination comme un nom de
  lien, jamais comme un répertoire à remplir ; exactement deux
  opérandes. Non combinable avec `-t`.
- `-h, -?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `ln -s /System/Commands/ls.app tools/ls` — lier un nom à un paquet.
- `ln -s ../shared/notes.txt` — lier `notes.txt` ici vers une cible
  relative.
- `ln -sv -t Links a.txt b.txt` — lier les deux fichiers dans `Links`
  en signalant chaque lien.
- `ln -sfn /Storage/media Music` — rediriger un lien `Music` existant
  vers un nouveau répertoire, en remplaçant le lien au lieu de lier à
  l'intérieur.

## EXIT STATUS

- `0` — tous les liens ont été créés (ou l'aide courte a été écrite) ;
  une question `-i` refusée n'est pas un échec.
- `1` — tout le reste, avec la raison affichée sur la sortie d'erreur.
  Une ligne de commande non comprise sort aussi avec `1`.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
