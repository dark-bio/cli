# Ark states

Discovery reports labels; the handshake establishes identity. Trust is attested,
self-signed, or pinned by --pubkey on status and enroll. A cloud environment
controls routing and never changes the handshake's trust result.

Status works offline, including while unpaired or locked. It reads pairing,
lock and cloud sync state without synchronizing. Commands that need cloud
requests reuse the Ark's identity and clock when fresh, synchronize when needed,
and attach the companion relay only when required. A CLI command owns its
connection until it exits. `genuine` and `doctor` explicitly refresh cloud sync.
Doctor reports checks and suggests fixes; it does not apply repairs.

In status, synced means cloud setup happened since boot and the Ark's clock is
within 15 seconds of this computer's. It is not a fresh registry check, so use
genuine for that. mismatch names the attested hardware when it disagrees with
what the Ark reports, and says nothing about dataset builds. Either field is
null when the firmware cannot report it.

identity is the fingerprint. pubkey is the full xDSA public key, several
kilobytes of hex, which text and JSON always keep whole. Human status shows the
fingerprint and prints the key with -v.

Two timestamps carry the same name. status firmware.published comes from the
running firmware, firmware list published from the package host, so they differ
for one version. In firmware list, candidate means installable rather than
newer, and a develop build can be a candidate for the version already
installed. update names the chosen candidate, or null when there is none.
Human mode labels that candidate update, which is not a pending update.

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
