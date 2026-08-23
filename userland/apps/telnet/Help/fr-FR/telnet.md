## NAME

telnet — le client de terminal virtuel réseau (RFC 854)

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Ouvre une connexion TCP vers un hôte et lui relaie le terminal : la sortie de
l'hôte apparaît sur la sortie standard, les frappes partent vers l'hôte, et le
caractère d'échappement (`^]` par défaut) ouvre l'interpréteur de commandes
`telnet>`. Sans hôte, `telnet` démarre à cette invite et `open` connecte.

C'est à la fois le moyen d'atteindre un service en mode ligne sur une autre
machine et le moyen d'interroger n'importe quel service TCP à la main —
`telnet host 80` ouvre une connexion dans laquelle vous pouvez taper une
requête.

L'hôte peut être un nom ou une adresse IPv4/IPv6 littérale. Un nom est résolu
par le résolveur trivial du système, qui lit les serveurs DNS récursifs
configurés via l'API d'information système. Le port est un nombre : il n'y a
pas de base de services, donc un *nom* de service est une erreur d'usage
plutôt qu'un repli silencieux sur le port 23.

La négociation des options suit la RFC 855 avec la discipline sans boucle de
la RFC 1143 : un pair qui se répète ne fait jamais se répéter le client. Les
options implémentées sont BINARY, ECHO, SUPPRESS GO AHEAD, STATUS, TIMING
MARK, TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW CONTROL, LINEMODE et
NEW-ENVIRON ; tout le reste est refusé, ce qui est le sens d'une option non
implémentée. LINEMODE (RFC 1184) est implémenté intégralement — le masque
`MODE`, la table des caractères locaux (SLC) et `FORWARDMASK` — de sorte que
le client édite la ligne comme le serveur le demande, avec les caractères que
le serveur négocie.

La taille de la fenêtre est signalée par NAWS à la connexion, puis à chaque
changement. TAIRiX n'a pas de signal de redimensionnement : la taille est
relue à chaque frappe, donc un redimensionnement atteint l'hôte à la frappe
suivante.

`NEW-ENVIRON` ne divulgue **que** les variables que vous définissez et
exportez avec la commande `environ` ; le client n'envoie jamais son propre
environnement. `-a` et `-l` exportent un nom de connexion, et c'est la seule
chose qu'une invocation divulgue d'elle-même.

Deux commandes de l'outil historique sont délibérément absentes. Il n'y a pas
d'échappement shell `!` : un programme qui analyse des données réseau
hostiles ne reçoit pas le droit de lancer un interpréteur. Il n'y a pas de
`slc check`, car la RFC 1184 ne lui donne aucune forme distincte de
`slc export`. Les données urgentes TCP ne sont pas exposées par l'interface
socket, donc un Synch voyage sous la forme du seul Data Mark. Lorsque
l'entrée standard atteint la fin de fichier — une invocation redirigée telle
que `telnet host 80 < requete` — le côté émission est fermé et la session
continue de lire jusqu'à ce que l'hôte distant ferme à son tour, de sorte que
la réponse n'est pas jetée comme le fait l'outil historique.

## OPTIONS

- `-4, --ipv4` — se connecter uniquement en IPv4.
- `-6, --ipv6` — se connecter uniquement en IPv6.
- `-8, --binary` — demander un chemin de données 8 bits dans les deux sens.
- `-L, --eight-bit-output` — demander un chemin 8 bits en sortie seulement.
- `-E, --no-escape` — aucun caractère d'échappement ; tout part vers l'hôte.
- `-e, --escape <char>` — définir le caractère d'échappement (`^]`, `^A`, un
  seul caractère, ou vide pour aucun).
- `-a, --login` — exporter le nom de connexion de la session via `NEW-ENVIRON`.
- `-l, --user <name>` — exporter `name` comme nom de connexion (implique `-a`).
- `-b, --bind <address>` — lier cette adresse locale avant de se connecter.
- `-d, --debug` — tracer la négociation des options sur la sortie d'erreur.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `telnet example.test` — ouvrir une session sur le port telnet assigné.
- `telnet 10.0.2.2 25` — dialoguer à la main avec un service de courrier.
- `telnet -6 fe80::2` — se connecter uniquement en IPv6.
- `telnet -l ada host` — proposer `ada` comme nom de connexion.
- `telnet -8 host` — demander un chemin 8 bits dans les deux sens.
- `telnet` puis `open host` — se connecter depuis l'invite de commandes.

## EXIT STATUS

- `0` — la session a eu lieu (quelle que soit la façon dont l'hôte l'a
  terminée), ou l'aide courte a été écrite.
- `1` — la session n'a pas pu avoir lieu : l'hôte n'a pas été résolu, le
  socket a été refusé, ou le terminal n'a pas pu passer en mode brut.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `TERM` — signalé à l'hôte via l'option TERMINAL TYPE.
- `USER` — le nom de connexion qu'exporte `-a`.
- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle
  que `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
