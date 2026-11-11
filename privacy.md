# Datenschutzerklärung

Diese Erklärung gilt für die privaten Anwendungen **Bibel**
(https://bible.eisler-systems.de) und **Aria** (https://aria.eisler-systems.de),
die denselben Google-Anmeldedienst (OAuth-Client) teilen.

**Verantwortlicher:** Eugen Eisler, erreichbar über eugen.eisler@siemens.com.
Die Anwendungen sind Privatanwendungen des eigenen Haushalts; es findet keine
gewerbliche Nutzung und kein Verkauf von Daten statt.

## Welche Daten verarbeitet werden

- **Anmeldedaten (lokales Konto):** E-Mail-Adresse und ein Hash des
  Passworts. Gespeichert ausschließlich auf dem privaten Heimserver.
- **Anmeldung mit Google:** Google übermitteln wir bei der Anmeldung Ihre
  E-Mail-Adresse, Ihr Anzeigename und Ihr Profilbild, um Ihr Konto zuzuordnen.
  Wir erhalten keinen Zugriff auf andere Google-Daten und speichern keine
  weiteren Angaben.
- **Passkeys:** Der private Schlüssel bleibt auf Ihrem Gerät; die Anwendung
  speichert nur die Kennung des Passkeys (Credential ID), um die Anmeldung zu
  ermöglichen.
- **Abfragen (Aria):** Sprach- und Textanfragen sowie die dazugehörigen
  Musik- und Predigtinhalte werden zur Beantwortung an einen externen
  KI-Anbieter (OpenRouter, z. B. DeepSeek) übertragen. Die Bibel-Anwendung
  beantwortet Anfragen lokal; Bibeltexte werden nicht an Dritte übertragen.
- **Server-Protokolle:** Technische Zugriffsdaten (IP-Adresse, Zeitstempel)
  werden im Rahmen des normalen Serverbetriebs kurzfristig protokolliert.

## Was NICHT passiert

- keine Analyse- oder Tracking-Dienste, keine Werbung, keine Cookies neben
  dem Sitzungstoken,
- keine Weitergabe oder Weiterverkauf von Daten an Dritte (ausgenommen die
  genannten Anmelde- bzw. KI-Dienste zur Erbringung der Funktion),
- keine Profilbildung über die Anwendung hinaus.

## Rechtsgrundlage und Speicherdauer

Die Verarbeitung beruht auf Ihrer Einwilligung (Art. 6 Abs. 1 lit. a DSGVO)
bzw. dem berechtigten Interesse am sicheren Betrieb der Anwendung
(Art. 6 Abs. 1 lit. f DSGVO). Daten werden gelöscht, sobald Sie Ihr Konto
löschen lassen oder die Anwendung stillgelegt wird.

## Ihre Rechte

Sie haben das Recht auf Auskunft, Berichtigung, Löschung und
Datenübertragbarkeit sowie das Recht, sich bei einer Datenschutz-Aufsichtsbehörde
zu beschweren. Wenden Sie sich dazu einfach an den verantwortlichen Betrieb
über die oben genannte Adresse.

## Sicherheit

Die Anwendungen sind nur über TLS (HTTPS) erreichbar. Passwörter liegen nur
als Hash vor, Anmelde-Token sind zeitlich begrenzt. Der Server befindet sich
im privaten Netz des Betriebs.
