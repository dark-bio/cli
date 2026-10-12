# Driving ark from an AI agent

An Ark holds one person's genomic data and runs apps on it. It is plugged
into this computer over USB, or emulated on it by Ark Emulator. Its owner
approves access to their data on their phone, in Ark Companion. You cannot
approve for them, so a command that needs approval waits until they act.

## Running

- Run one ark command at a time, reads included. An Ark serves one session,
  so a second command fails with device-busy, and so does any command while
  an Ark Hub browser tab holds the Ark.
- Add --json to parse a result. stdout then holds one complete document and
  stderr one JSON event per line. The default output is for reading, and may
  shorten or round values.
- ark never prompts without a terminal, or under --json or --no-input. Its
  two prompts have flags instead. --unlock unlocks a locked Ark first, and
  --yes on `ark firmware update` confirms the installation. Without them the
  command fails and names the flag.
- Every command blocks until its outcome is known, and the exit code is that
  outcome. A command that waits on the owner or on a large transfer takes
  minutes, so run it in the background. Use your harness's background mode
  where it has one. Otherwise keep the streams and the exit code in files:

      rm -f exit.txt
      (ark --json app run app.wasm > result.json 2> events.log
       echo $? > exit.txt) &

  Read events.log every few seconds. The command has ended once exit.txt
  exists, and result.json then holds its document.
- Never let a time limit kill a running command. A task belongs to the
  command that started it and ends with it. Ctrl-C or SIGTERM asks the Ark
  to cancel the task or upload in flight, then exits 130 or 143. An upload
  the Ark is already processing keeps going.
- --timeout, 60s unless set, bounds each wait on a machine, such as a reply
  from the Ark or the next chunk of a download. It never bounds an approval
  or a whole command, and opening a connection has its own limit of 5s. A
  cap on a whole command is yours to add.

## Approvals

These commands wait on the owner, for at most the window the Ark opens:

- `ark unlock` waits up to 2 minutes for an approval on the phone. So does a
  data or app command, or `ark firmware update`, given --unlock while the
  Ark is locked.
- `ark pair` waits up to 10 minutes for the owner to open its pairing link
  in Ark Companion. The Ark and the phone then show colors, and the owner
  presses the Ark's button with its pin within 60s when they match.
- `ark data upload` waits up to 1 minute for an approval on the phone, since
  it uploads the owner's own calls. References come through
  `ark data fetch`, which needs none.
- `ark data delete` and `ark data repair` wait up to 2 minutes for an
  approval on the phone.
- `ark firmware update` needs nothing while the Ark is unpaired. A paired,
  locked Ark waits 30s for a press of its button, and an unlocked one up to
  2 minutes for the phone.
- `ark app run` waits on the phone twice. First the owner approves the run
  within 5 minutes, seeing the app's name, version and what it reads. There
  they switch on any optional grants and answer the app's questions. Once
  the app ends, they review its report within 10 minutes, then release it to
  your command or decline, which drops it from the Ark.

Before you start such a command, let the person know what they will approve
and where, so their phone or the Ark is at hand. Then pass on each approve
event the moment it arrives, quoting its message. It is a line starting
with approve:, or with `{"event":"approve"` under --json. The phone's
notification may not arrive, and opening Ark Companion shows the request
anyway.

A declined approval fails the command with approval-denied, and one that
ran out with approval-timeout, both exit 6. approval-undelivered, exit 4,
means the request never reached the phone. Ask the person before running the
command again, since each run asks them again.

## Reading results

- The exit code is the outcome. 0 is done, 1 a local input or a missing
  confirmation, 2 usage, 3 reaching the Ark, 4 the cloud, 5 the Ark's state
  or refusal, 6 an approval that did not come, 7 a machine wait that ran
  out, 8 an app failure, 130 Ctrl-C and 143 SIGTERM.
- A failure prints error[code]: message, then hint: lines naming the next
  step. Codes are stable, and `ark help output` lists every one with its
  next step. error[ark] is the Ark's own refusal. Act on its message, never
  on its number, which can change between firmware builds.
- A command that did part of its work keeps that result on stdout and still
  exits non-zero. Under --json, a command with no result prints a document
  holding only the error.
- A note that a newer ark is available repeats on every command until ark is
  upgraded. Pass it on to the person, since upgrading is their call.

## Checking state

`ark devices` lists the Arks this computer sees, without connecting.
`ark status` connects to one and shows its identity, firmware, pairing and
lock state. It works offline and in any state. These steps, in this order,
bring an Ark to where data and app commands work:

1. Firmware older than this tool supports fails with firmware-outdated.
   `ark firmware update` fixes that on hardware, and a newer Ark Emulator
   on an emulator.
2. A self-signed Ark, such as a fresh emulator, is enrolled first.
   `ark enroll` prints the Ark Hub address where the person enrolls it in a
   browser, and exits 1 with enrollment-required.
3. An unpaired Ark is paired once, through `ark pair` and the owner's phone.
4. A locked Ark is unlocked through `ark unlock` and the owner's phone. It
   stays unlocked until it loses power.

Add --unlock only when status reports locked and the person asked for work
that needs the Ark unlocked. A read-only task never adds it, and a dry run
never unlocks.

Nothing in ark waits for an Ark to appear. To wait for one, poll
`ark status --json`, which fails with no-device until an Ark is attached.

Never propose a reset or deleting the pairing in Ark Companion as a way past
a locked Ark. Holding the Ark's button for 5s arms a reset, which erases its
data once the button is let go, and deleting the pairing discards the key
that unlocks it. The button sits behind a pinhole under the bottom right
LED, as `ark help devices` describes.

## Tasks

### Run an app

Check `ark status` first. Let the person know that the run asks them twice
on the phone, then run this in the background, the way Running shows:

    ark --json app run app.wasm > result.json 2> events.log

The report is the document's stdout field. Without --json, ark writes the
report itself to stdout, so `> report.md` keeps it as the app printed it.
`ark help apps` covers the whole run.

### Write an app

`ark help apps` covers what an app is, its manifest and how the Ark checks
it. `ark data paths --json` describes every path an app can read on this
Ark. https://github.com/dark-bio/examples has worked apps in Rust, Go, C and
Python, with fixtures that run them on this computer. ark has no app
runtime of its own, so an app runs on an Ark, hardware or emulated.

### Load data

`ark data list` shows each slot and what it requires. The owner's variant
calls come first, through `ark data upload FILE` and an approval on the
phone. `ark data fetch --all` then downloads the public references the Ark
offers for the calls' assembly, without an approval, and skips any it
offers none for. The variant catalog alone takes an hour or more.
`ark help datasets` covers both.

### Update firmware

`ark firmware update --dry-run` shows the plan and who approves it.
`ark firmware update --yes` installs it, then waits up to 120s for the Ark
to come back running it. `ark help firmware` covers the rest.

### Use an emulator

Without hardware, Ark Emulator from https://github.com/dark-bio/emulator
boots the real firmware on this computer. Read `ark-emulator help agents`
before starting one. `ark-emulator start` prints the locator that --device
takes, and the steps under Checking state apply from there.

### Diagnose

`ark doctor` checks this computer, the Ark and the cloud, and suggests fixes
without applying them.
