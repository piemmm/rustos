## NAME

ss — lister les sockets ouverts

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Liste les sockets ouverts du système, une ligne par socket : le
protocole de transport, l'état de la connexion, la profondeur des files
de réception et d'émission, l'`address:port` local et distant, et — avec
`-p` — le processus propriétaire.

Les lignes proviennent de la liste des sockets de l'API d'informations
système, que la pile réseau traite comme une requête privilégiée et
auditée : elle nomme les sockets de chaque principal et le pair de
chaque connexion, si bien que lister tous les sockets exige
`CAP_SYSINFO_GLOBAL`. Il n'y a pas de `/proc/net` ; une session sans
cette capacité en est informée et `ss` se termine, plutôt que d'afficher
une table vide.

Par défaut, la liste montre les sockets connectés, non en écoute. `-l`
ne montre que les sockets en écoute et `-a` montre les deux ; le nombre
d'entrées en écoute masquées est noté sur le flux d'information standard
(fd 3), jamais dans la table. `-t` et `-u` restreignent le protocole et
`-4`/`-6` la famille d'adresses ; sans aucun, tout protocole et toute
famille sont affichés. Les ports sont toujours numériques (TAIRiX n'a
pas de base de noms de services), donc `-n` est accepté mais toujours
actif pour eux. Les adresses le sont aussi, sauf si `-r` demande des
noms d'hôtes : `-r` résout chacune via le résolveur du système (une
requête `PTR`), n'interroge chaque adresse distincte qu'une fois, et
laisse numérique celle qui n'a pas de nom. Une adresse non spécifiée
s'affiche `*` et un port non lié `*` ; une adresse IPv6 est entre
crochets pour que le séparateur `:port` reste sans ambiguïté — un nom
résolu n'a pas besoin de crochets.

`ss` n'accepte que des options. La grammaire d'expressions de filtre
d'iproute2 (filtres d'état et d'adresse) n'est pas implémentée, donc un
opérande nu est une erreur d'usage plutôt qu'un argument silencieusement
ignoré.

## OPTIONS

- `-t, --tcp` — montrer les sockets TCP. Sans `-t` ni `-u`, les deux
  protocoles sont montrés.
- `-u, --udp` — montrer les sockets UDP.
- `-a, --all` — montrer les sockets en écoute et connectés.
- `-l, --listening` — ne montrer que les sockets en écoute.
- `-n, --numeric` — ne pas résoudre les noms de services. Toujours
  actif sur TAIRiX ; accepté par familiarité. Les noms d'hôtes sont
  l'affaire de `-r`.
- `-r, --resolve` — résoudre les adresses en noms d'hôtes via DNS.
  Désactivé par défaut : la liste n'émet aucune requête sans demande.
- `-p, --processes` — ajouter la colonne du processus propriétaire
  (`pid=N`).
- `-4, --ipv4` — restreindre la liste aux sockets IPv4.
- `-6, --ipv6` — restreindre la liste aux sockets IPv6.
- `-H, --no-header` — supprimer la ligne d'en-tête.
- `-s, --summary` — afficher les totaux de défense des connexions TCP
  de la pile au lieu de la table des sockets.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `ss` — les sockets connectés, non en écoute.
- `ss -a` — tous les sockets, en écoute et connectés.
- `ss -l` — seulement les sockets en écoute.
- `ss -tlp` — les sockets TCP en écoute, avec le processus propriétaire.
- `ss -u4` — les sockets UDP sur IPv4.
- `ss -r` — la même liste, adresses résolues en noms d'hôtes.

## EXIT STATUS

- `0` — la liste a été produite (ou l'aide courte a été écrite).
- `1` — la requête de sockets a été refusée ou a échoué, ou la sortie
  n'a pu être écrite.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
