# Finding and selecting an Ark

`ark devices` discovers hardware over USB and running emulators through their
local launcher registry. It does not connect or authenticate. A missing emulator
registry is normal when no emulator is running. A failed discovery source does
not hide devices found through another source.

Select an exact locator (hardware:BUS:ADDR or emulator:PORT), unique serial,
name or emulator image basename. The words hardware and emulator select the
only device of that kind. There are no prefix matches or case folding; locator
prefixes and the two kind names cannot be shadowed by labels.

USB addresses may change across reboot. Firmware verification searches by the
reported serial, then authenticates the same identity key and checks the build.
A discovery serial alone never proves which Ark returned.

The environment order is --env, trusted attestation, emulator launcher report,
then release. The CLI trusts release, staging and develop roots. An explicit
--env that contradicts the attestation warns; it changes routing, not trust.
Develop and staging routes produce one note per command, hidden by --quiet.
The offline attested label identifies the signer. Use genuine to check the
Ark's current cloud registration.
