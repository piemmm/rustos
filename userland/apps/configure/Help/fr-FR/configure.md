## NAME

configure — lire et régler la configuration système au démarrage

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Liste, affiche et règle les paramètres du magasin de configuration
situé à `/System/Settings/Configuration/system.conf`. Sans opérande,
chaque paramètre est listé avec sa valeur actuelle ; avec une clé
seule, la valeur de ce paramètre est affichée ; avec une clé et une
valeur, le paramètre est modifié.

Le magasin réside sur le volume racine chiffré et n'est lu par ses
consommateurs qu'après le déverrouillage du système de fichiers
racine ; une modification prend donc effet au prochain démarrage de son
consommateur (`os.loginType` : la connexion du prochain démarrage ;
les commutateurs `cache.*` : le déverrouillage du prochain démarrage).

L'ensemble des clés est fermé : une clé inconnue, ou une valeur hors de
l'ensemble d'une clé, est refusée avec l'énoncé des choix valides et ne
change rien. Modifier un paramètre réécrit le magasin sous sa forme
canonique et exige le droit d'écriture sur `/System/Settings` — un
compte ordinaire peut lire les paramètres mais pas les changer.

- `os.loginType` — `text` ou `graphical` : le type de session que le
  service de connexion lance pour un utilisateur authentifié. `text`
  (la valeur par défaut) lance le shell du compte — le bureau peut
  toujours être lancé à la demande avec la commande `desktop` ;
  `graphical` lance directement la session de bureau après
  l'authentification quand un bureau est installé, et se replie sur le
  texte sinon.
- `cache.all` — `on` ou `off` : le commutateur de cache principal. `on`
  (la valeur par défaut) laisse chaque classe de cache ci-dessous
  suivre son propre réglage ; `off` est un plafond qui désactive tout
  cache en mémoire quels que soient les réglages par classe.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` ou `off` : les commutateurs par classe pour
  les quatre caches mémoire récupérables (les caches du système de
  fichiers, du bloc disque entier, du cluster décompressé et du
  lancement d'applications). `auto` (la valeur par défaut) laisse le
  gestionnaire de pression mémoire gouverner la classe ; `off` la
  désactive entièrement. Il n'y a pas de `on` par classe : une classe
  ne peut pas être forcée à ignorer la pression mémoire. Une classe est
  effectivement `off` dès que `cache.all` est à `off`.

Chaque cache est un accélérateur récupérable, jamais la source de
vérité ; désactiver l'un d'eux ou tous ne fait donc que ralentir le
travail concerné — cela ne change jamais un résultat.

- `net.ipv4.enabled`, `net.ipv6.enabled` — `true` ou `false` : les
  commutateurs de familles d'adresses à l'échelle de la pile. Les deux
  valent `true` par défaut. Une famille désactivée n'attribue aucune
  adresse, ne répond à aucun paquet et refuse un socket de cette
  famille par une erreur typée — jamais un rejet silencieux.
- `net.ipv6.privacy` — `true` ou `false` : si la pile forme des
  adresses IPv6 temporaires (de confidentialité) en plus de l'adresse
  stable. `false` (par défaut) n'utilise que l'adresse SLAAC stable.
- `net.tcp.syncookies` — `auto` ou `always` : la défense contre les
  inondations SYN. `auto` (par défaut) conserve une file semi-ouverte
  bornée et bascule vers des cookies sans état en cas de débordement ;
  `always` répond à chaque demande de connexion sans état. Il n'y a
  pas de `off` — une file de connexions non défendue n'est pas un
  réglage.

La pile réseau lit les réglages `net.*` ; une modification prend effet
lorsque la pile applique de nouveau sa configuration.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `configure` — lister tous les paramètres.
- `configure os.loginType` — afficher le type de session par défaut.
- `configure os.loginType graphical` — démarrer sur la connexion
  graphique.
- `configure cache.all off` — désactiver tout cache en mémoire sur tout
  le système.
- `configure cache.filesystem off` — désactiver uniquement le cache du
  système de fichiers.

## EXIT STATUS

- `0` — la liste, la valeur, l'aide courte ou la modification a été
  effectuée.
- `1` — le magasin n'a pas pu être lu ou écrit (par exemple l'appelant
  ne peut pas modifier les réglages système), ou la sortie n'a pas pu
  être délivrée.
- `2` — la ligne de commande n'a pas été comprise, la clé est inconnue
  ou la valeur est hors de l'ensemble de la clé.

## ENVIRONMENT

- `LANG` — la langue préférée de l'aide courte (une étiquette BCP-47
  comme `fr-FR`).

## SEE ALSO

- `man`
