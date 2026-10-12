# Devices

Each ark command works with one Ark. It finds the Arks attached to this
computer without connecting, picks one, then connects and proves the Ark's
identity before its first request.

## Finding and choosing an Ark

`ark devices` lists hardware Arks on USB and the emulators that the emulator
launchers on this computer report. It never connects, so the names and
serials it shows are what discovery reported, not verified facts. The
reading view shows locator, name, serial and kind, and environment and
ready when an emulator reports them. --json adds image, an emulator's image
file name.

The environment and ready fields come from the emulator launcher. They are
null for hardware, and a null there does not mean the Ark is unreachable. A
discovery source that fails prints a warning, and the Arks the other source
found still show. Having no emulator running is normal.

One attached Ark is picked on its own. With several, a command fails with
ambiguous-device and lists their locators, and --device picks one by:

- its locator, hardware:BUS:ADDR or emulator:PORT
- its serial, name or emulator image name, when it is unique
- the word hardware or emulator, when only one Ark of that kind is attached

A selector matches exactly, without prefixes or case folding. USB addresses
can change when an Ark reboots, and a serial from discovery never proves
which Ark answered, since only the handshake does.

An Ark serves one session at a time. While another ark command or an Ark
Hub browser tab holds the Ark, a command fails with device-busy. Wait for
the other command or close the tab, since ark never retries a busy Ark on
its own.

On Linux, reaching an Ark over USB needs a udev rule, which the hint of a
permission error spells out.

## Identity and trust

Every connection starts with a handshake that proves the Ark holds its
identity key, within 5s. ark trusts that key in one of three ways, which
`ark status` reports as trust:

- attested, by an attestation from Dark Bio's release, staging or develop
  root, which also vouches for the Ark's serial, realm and model
- self-signed, as on a fresh emulator before `ark enroll`
- pinned, when --pubkey on `ark status` or `ark enroll` names the key to
  accept, for an Ark whose attestation cannot be verified

`ark status` reads the attestation without the cloud. `ark genuine` asks
Dark Bio's device registry whether the Ark is enrolled and active. It prints
serial, enrolled, active, disabled, expired and superseded, and fails with
registry-inactive when the registration is disabled, expired or superseded.

## Pairing and unlocking

An Ark is paired once, with the owner's phone, then unlocked after every
power loss.

`ark pair` opens a pairing window of 10 minutes and prints a link. At a
terminal ark also draws it as a QR code. Elsewhere the link arrives in an
approve event, for the person to open on the phone that runs Ark Companion.
The Ark and the phone then show colors, and the owner presses the Ark's
button within 60s when they match. The Ark then prepares its encrypted
storage. A paired Ark refuses `ark pair` with already-paired.

`ark unlock` asks the owner's phone and waits up to 2 minutes. The Ark then
stays unlocked until it loses power. Every data and app command needs a
paired, unlocked Ark. At a terminal such a command offers to unlock first.
Elsewhere it fails with locked, unless --unlock lets it unlock first. A dry
run never unlocks.

The Ark's button sits behind a small pinhole under its bottom right LED, to
the right of the USB cable, and is pressed with the pin that came with the
Ark. A short press confirms a pairing, or a firmware update on a locked Ark.
Holding it for 5s arms a reset, which erases the Ark's data and its pairing
once the button is let go. An emulated Ark has its button in the Ark
Emulator window. Deleting the pairing in Ark Companion discards the key that
unlocks the Ark. Neither the reset nor deleting the pairing is a way past a
locked Ark.

## What status reports

`ark status` shows what the Ark is, how it is trusted and its cloud, pairing
and lock state. It hints at the next step, such as `ark enroll`, `ark pair`
or `ark unlock`. With --json the document holds:

- name, the name discovery reported
- serial, realm and environment, from the attestation, so null on an Ark
  without one; the reading view shows such a serial as unverified
- hardware, with version and revision as the Ark reports them and model
  from the attestation
- firmware, with version and published, the running build's publish time
- trust, which is attested, self-signed or pinned
- synced, whether the Ark synced with the cloud since it booted and its
  clock is within 15s of this computer's; it is not a registry check
- paired and unlocked
- identity, the key's fingerprint, and pubkey, the whole key in hex, which
  only --json carries
- mismatch, the attested hardware version when the Ark reports another one,
  and null otherwise

On firmware older than this tool supports, synced, paired and unlocked are
null, and `ark status` exits 5 with firmware-outdated after printing them.

## Emulated Arks

Ark Emulator, from https://github.com/dark-bio/emulator, boots the real
firmware on this computer, for development and demos. It keeps its data in
a plain file, so real data belongs on hardware. `ark-emulator start` boots
one and prints its locator once it accepts clients, and `ark devices` lists
it from then on. `ark-emulator help agents` covers starting, stopping and
wiping emulators.

ark talks to an emulator as to hardware, apart from its identity and its
firmware. A fresh emulator is self-signed, and is enrolled before it pairs.
`ark enroll` prints the Ark Hub address where the person enrolls it in a
browser, which gives it an attested identity for 30 days. After that the
cloud refuses it, and `ark genuine` reports it expired.
`ark-emulator stop`, `ark-emulator wipe` and `ark-emulator start` then give
a fresh device to enroll again. The firmware is the build bundled with Ark
Emulator, as `ark help firmware` describes.

`ark enroll --cwt FILE` installs an attestation from a file instead, then
reconnects to check the identity it carries.

## Cloud environments

Most requests need the Ark synced with the cloud since it booted. When
status reports synced, ark reuses that sync, so reads and dry runs work
offline. Otherwise ark syncs on the first request that needs it, which takes
the cloud. Approvals reach the phone through the cloud, and `ark genuine`,
`ark pair` and the firmware commands always talk to it. `ark devices`,
`ark status` and `ark enroll --cwt` never do.

An Ark belongs to one cloud environment, release, staging or develop, which
picks the cloud and package hosts ark talks to for it. ark takes it from
--env, then from the Ark's attestation, then from the emulator launcher, and
otherwise uses release. An emulator image is bound to its environment when
it first boots. --env changes where requests go, never how the Ark is
trusted, and overriding the attestation prints a warning. A command against
staging or develop notes the environment once.

Staging and develop hosts need a team login. At a terminal, ark opens a
browser to sign in through cloudflared. Without a terminal, or under --json
or --no-input, the command fails with login-required, and its hint gives the
cloudflared command to run.
