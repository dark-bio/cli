# Firmware

An Ark runs ArkOS, its firmware. `ark firmware list` shows the build the Ark
runs and the published builds it can move to. `ark firmware update` installs
one and waits for the Ark to come back running it. An emulated Ark runs the
firmware bundled with Ark Emulator instead, so a newer emulator release is
its update.

## Compatibility

This tool works with firmware from a minimum version on, and develop
firmware also needs a publish time at or after a cutoff, whatever its
version. Against older firmware every command fails with firmware-outdated,
except the ones that lead out of it:
`ark devices`, `ark status`, `ark doctor`, `ark firmware list`,
`ark firmware update` and `ark enroll --cwt`. Below the minimum version the
error names it. `ark status` still prints what it reads, with sync, pairing
and lock null, then exits 5.

A firmware that answers unknown does not know the request, so the firmware
and this tool are out of step. Update both.

## Listing builds

`ark firmware list` reads the builds published for the Ark's cloud
environment and shows the installed build and every candidate. A candidate
is a build with a higher version. When the installed build is a develop
build, every build of the same version is a candidate too. The reading view
lists the builds newest first, with version, publication date, size and
status, installed or update. Below the table, each candidate's headline,
the first paragraph of its summary, prints whole under its version. The
installed build's headline stays out.

With --json the document holds:

- installed, the version the Ark runs
- update, the version `ark firmware update` would pick, or null
- firmwares, the installed build and the candidates, each with version,
  published, size_bytes, sha256, summary, installed and candidate

A build's published time is when the package host published it, which can
differ from the publish time `ark status` reads from the running firmware.
Its summary is the whole release note.

## Updating

`ark firmware update` installs the newest candidate. Given a version, as
`ark firmware list` prints it, it installs that published build instead,
which may reinstall the running build or go back to an older one, if the Ark
accepts it. With nothing newer
published, it notes so and exits 0. --dry-run prints the plan and changes
nothing. The plan holds from, to, size_bytes and approval, which is none,
button or phone, or null on firmware too old to report the Ark's state.

An installation runs in four steps:

1. Confirmation on this computer. A terminal asks for it. Elsewhere --yes
   gives it, and without --yes the command fails with confirmation-required.
2. The owner's approval, which depends on the Ark's state. An unpaired Ark
   needs none. A paired, locked Ark wants a press of its button within 30s,
   with the pin through the pinhole under its bottom right LED. An unlocked
   Ark asks the owner's phone, with 2 minutes to approve.
   --unlock unlocks a locked Ark first, so the phone approves the update
   instead of the button.
3. The transfer. ark downloads the build from the package host and streams
   it to the Ark, which verifies it, installs it and reboots.
4. The return. ark waits up to 120s for the Ark to come back. It checks
   that the Ark holds the same identity key, and compares the running
   version with the one it installed.

In the result, installed turns true once the Ark accepts the build,
returned once the Ark comes back with the same identity key, and verified
once it runs the new version. The running field names the version it runs.
A different version fails with update-unverified, and no return within
120s with timeout. --no-wait skips the return and exits once the
installation is acknowledged, with running null.

An update can fail with proof-rejected when the cloud's keys have changed.
ark refreshes them before it exits, so run the command again.

## Emulated Arks

An emulated Ark refuses firmware updates with unsupported. Its firmware is
the build bundled with Ark Emulator, and `ark-emulator --version` names it.
A newer Ark Emulator release carries newer firmware.
