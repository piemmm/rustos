## NAME

cinder — ein Desktop-Begleiter, der in einem Laufstall wohnt und den Desktop durchstreift

## SYNOPSIS

`cinder`

## DESCRIPTION

Öffnet ein kleines Laufstall-Fenster mit Cinder darin. Cinder ist das TAIRiX-
Maskottchen: ein kleines rost- und anthrazitfarbenes Wesen, das herumstreift,
sich putzt, döst und bemerkt, wo Ihr Zeiger ist.

Man kann ihn hinauslassen. Wählen Sie **Cinder hinauslassen** in seinem Menü in
der Symbolleiste, und er verlässt den Laufstall für den Desktop selbst, wo er
zwischen Ihren Fenstern umherwandert. Trifft er auf eines, klettert er hinauf
und setzt sich auf die Titelleiste, macht sich flach und schlüpft darunter
hindurch, oder geht schlicht darum herum — was er tut, hängt von der Form des
Fensters und von seiner Laune ab. Bewegen Sie den Zeiger in seine Nähe, so
beobachtet er ihn, trabt ihm nach und springt ihn an, wenn er ihn einholt.

Klicken Sie ihn an, um ihn zu streicheln, im Laufstall wie draußen. Streicheln
heitert ihn auf. Draußen fängt nur er den Klick ab: der durchsichtige Raum um
ihn herum gehört dem, was dahinter liegt, sodass ein Begleiter, der über Ihrer
Arbeit sitzt, nie einen Klick verschluckt, der ihr galt.

Im Laufstall können Sie ihn auch hochheben und irgendwo auf dem Boden wieder
absetzen und den Ball herumschubsen.

**Das Menü.** **Cinder hinauslassen** / **Cinder heimholen** wechselt, wo er ist.

**Beenden** beendet ihn und nimmt ihn vom Desktop.

**Den laufstall schliessen.** Das Schließen des Laufstall-Fensters beendet nichts. Cinder ist eine residente
Anwendung: schließen Sie den Laufstall mit ihm darin, wird er weggeräumt;
schließen Sie ihn, während Cinder draußen ist, streift er weiter umher. Klicken
Sie sein Symbol in der Leiste an, um den Laufstall wieder zu öffnen. *Beenden*
ist das, was ihn beendet.

**Laune.** Cinder will dreierlei: Ruhe, Spiel und Gesellschaft. Herumlaufen verbraucht
seine Energie, ein Nickerchen stellt sie wieder her; Stillsitzen langweilt ihn,
das Jagen des Zeigers unterhält ihn; Streicheln heitert ihn auf. Sie müssen
nichts davon verwalten — es gibt nichts zu füttern, und nichts geht schief, wenn
Sie ihn in Ruhe lassen. Es ist da, damit Sie auf einen Blick sehen, in welcher
Laune er ist.

Wie es ihm geht und ob er draußen war, bleibt zwischen den Sitzungen erhalten.

**Wenn er nicht hinaus kann.** Auf dem Desktop außerhalb eines Fensters zu sein erfordert die Berechtigung
`CAP_DESKTOP_LAYER`, die diese Anwendung in ihrem signierten Manifest anfordert
und die auch die Rechte Ihres Kontos zulassen müssen. Tun sie das nicht — oder
gibt es keine grafische Sitzung — nennt **Cinder hinauslassen** den Grund im
Laufstall und auf der Standardfehlerausgabe, und der Laufstall arbeitet weiter.
Cinder bleibt einfach drinnen.

## EXIT STATUS

`0` bei sauberem Beenden. Ein Status ungleich null wird stets von der
Begründung auf der Standardfehlerausgabe begleitet.

## SEE ALSO

`sapper`
