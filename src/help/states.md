# Ark states

Discovery reports labels; the handshake establishes identity. Trust is attested,
self-signed, or pinned by --pubkey on status and enroll. A cloud environment
controls routing and never changes the handshake's trust result.

Status works offline and shows pairing, lock and cloud sync state. Connect
reuses the Ark's cloud identity and clock when fresh, synchronizes when needed,
and attaches the companion relay only when required. A CLI command owns its
connection until it exits. `genuine` and `doctor` explicitly refresh cloud sync.

`ark --version` prints the minimum supported firmware and the develop image
timestamp cutoff. Develop images built before the cutoff need rebuilding or
updating; tagged builds use the version minimum alone. Older devices support
only status, doctor, firmware commands and enroll --cwt. Status prints unknown
sync state, then firmware-outdated. Update hardware with `ark firmware update`;
for an emulator, update the emulator app.

An unpaired Ark becomes paired through `ark pair`, then can be unlocked through
`ark unlock`. Unlocking lasts until power is cut. Data commands and app uploads
require unlocked state. --unlock authorizes that extra step; --dry-run never
unlocks, and conflicts with --unlock. A dry run requiring unlocked state asks
you to unlock separately.

Firmware updates on unpaired Arks need no phone or button approval. Paired,
locked Arks use the device button; unlocked Arks use the phone. --unlock can
switch a locked update to the phone path. The CLI still requires confirmation
of installation and reboot; use --yes noninteractively. The Ark makes the
final decision about every operation.
