## NAME

widgets — Reactive-Alloy-Widget-Galerie

## SYNOPSIS

`widgets`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das jedes gemeinsame GUI-Steuerelement von TAIRiX
auf einem eigenen Reiter vorführt: Schaltflächen, Auswahlschalter,
Wertsteuerelemente, Textfelder, Auswahlsteuerelemente, Sammlungen, Leisten,
Rückmeldeflächen und Fenstersteuerelemente. Jeder Reiter zeigt mehrere
Varianten seiner Familie — verschiedene Rollen, Zustände und Werte —, sodass
das vollständige Verhalten jedes Steuerelements an einer Stelle sichtbar und
bedienbar ist.

Wechseln Sie den Reiter durch Klicken auf die Reiterleiste oder mit den Tasten
`Left`, `Right`, `Home` und `End` sowie `Enter`. Klicken Sie ein Steuerelement
an, um es zu bedienen: ein Schalter kippt um, ein Schieberegler bewegt sich,
ein Textfeld erhält den Cursor, ein Auswahlfeld öffnet sich. Ein angeklicktes
Steuerelement behält den Tastaturfokus, sodass die Pfeiltasten, `Enter`,
`Space` und getippte Zeichen es dann steuern; `Tab` und `Shift+Tab` bewegen den
Fokus zwischen der Reiterleiste und den Steuerelementen.

Die Galerie wird über das Startmenü des Desktops oder namentlich aus einer
Shell gestartet. Sie erfordert eine laufende grafische Sitzung: ohne sie ist
der Fensterkanal nicht erreichbar, und die Galerie meldet die Ablehnung auf dem
Standardfehlerstrom und beendet sich.

## EXIT STATUS

Null nach einem sauberen Schließen; ungleich null, wenn der Fensterkanal oder
die gemeinsame Rahmenregion abgelehnt wurde (der Grund wird auf dem
Standardfehlerstrom angegeben).
