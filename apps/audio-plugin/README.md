# Oink virtual microphone

The Audio Server (HAL) plugin exposes **Oink Microphone** and a hidden feed
output. The bundle/executable are `OinkMicrophone.driver` / `OinkMicrophone`,
with factory `OinkMicrophoneFactory` and bundle ID `com.hachiwii.oink.microphone.driver`.
Device UIDs are `com.hachiwii.oink.microphone.input.v1` and `.feed.v1`; the model
UID is `com.hachiwii.oink.microphone.v1`. The input remains eligible as the default
input; the feed remains hidden. Audio formats and the read-only `AcID` ABI are unchanged.

One installation-only migration exception recognizes the shipped
`AcrossMicrophone.driver` as outdated. Explicit installation removes that old
bundle in the same privileged operation after staging/verifying the new driver;
uninstall removes either name. Backups stay outside HAL. Audio routing never
falls back to legacy UIDs. See `docs/design/mic-plugin-installation.md`.

`build.sh <absolute build directory> <source-tree SHA256>` builds and runs the
loopback and identity tests. The identity test loads the driver into its own
process, without HAL registration or StartIO. Neither test installs anything.
`packaging/macos/build.py` builds this component and signs it using the existing
Developer ID Application flow (team `893V37X2CW`), before signing the outer app.
The pinned libASPL dependency and all three upstream license texts are recorded
in the native license inventory.

Installation and removal are explicit local user actions. The agent verifies the
outer application's seal, fixed identifiers/team, installer and source-tree
provenance before asking macOS for administrator authorization. The embedded
bootstrap copies the application to a private root-owned staging directory,
verifies that immutable copy again, and executes its sealed installer. Installation
restarts system audio and briefly interrupts calls. No startup, discovery or
network message can invoke the installer.

An already installed old plugin is left untouched until the user chooses an
operation. Normal virtual input operation uses only CoreAudio I/O and read-only
consumer observation; microphone hardware capture remains demand-driven.
