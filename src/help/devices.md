# Finding and selecting an Ark

`ark devices` discovers hardware over USB and running emulators through their
local launcher registry. It does not connect or authenticate. A missing emulator
registry is normal when no emulator is running. A registry whose listing
version this tool does not know is a failed source, reported as a warning. A
failed discovery source does not hide devices found through another source.

Emulators come from the desktop app at https://github.com/dark-bio/emulator,
which boots the real firmware on this computer. It exists for development and
demos and keeps its data in a plain file, so keep real data on hardware.

The reading view shows locator, name, serial, kind, environment and ready.
JSON devices entries contain locator, kind, name, serial, image, environment
and ready. environment and ready are launcher metadata for
emulators and normally null for hardware, because USB discovery does not
handshake. Other unavailable labels are null too. The reading view shows -;
this does not mean unreachable or uninitialized. Use status for authenticated
state, and do not gate hardware access on discovery's ready.

Select an exact locator (hardware:BUS:ADDR or emulator:PORT), unique serial,
name or emulator image basename. The words hardware and emulator select the
only device of that kind. There are no prefix matches or case folding; locator
prefixes and the two kind names cannot be shadowed by labels.

USB addresses may change across reboot. Firmware verification searches by the
reported serial, then authenticates the same identity key and checks the build.
A discovery serial alone never proves which Ark returned.

## Cloud environments

The environment order is --env, trusted attestation, emulator launcher report,
then release. The CLI trusts release, staging and develop roots. An explicit
--env that contradicts the attestation warns; it changes routing, not trust.
Develop and staging routes produce one note per command, hidden by --quiet.
The offline attested label identifies the signer. Use genuine to check the
Ark's current cloud registration.

Develop and staging cloud and package hosts may require Cloudflare Access login.
Install cloudflared when prompted. Interactive commands open a browser when login
is needed and reuse the session afterward. Without a terminal, under --no-input,
or with --json, login-required includes the manual login command. API and
package hosts have separate credentials. Status remains usable offline.

Firmware checks cloud access before preparation. If login expires after the Ark
has prepared the update, the CLI signs in and asks you to rerun the command;
it never repeats a possible device approval automatically.
