# Driving ark from a script or an AI agent

An Ark holds one person's health data. It is plugged into this computer over
USB, or emulated on it by Ark Emulator. The owner approves access to their data
on their phone, in Ark Companion. You cannot approve for them.

## Running

- Use --json for complete, exact data: one indented JSON document on stdout,
  JSON Lines events on stderr. Default output is formatted for reading and
  may scale values or omit fields. If your tool merges the streams, drop the
  lines starting with `{"event":`; each event is one line and what remains is
  the document. `ark help output` defines the stream and error contracts.
- Run ark commands one at a time, including reads. Concurrent commands to the
  same Ark collide with device-busy; wait for your earlier command to finish.
  One Ark is selected automatically. With several, select an exact locator or
  unique label with --device. A browser tab can also hold the USB session.
- Nothing prompts when stdin is not a terminal, under --no-input, or with
  --json. --yes confirms firmware installation; without it the command fails
  with confirmation-required. Develop and staging hosts may need browser
  login; without a terminal, login-required gives the command to run.
  The owner still approves operations on the Ark when required.
- Every command blocks until done and its exit code is the outcome. Run long
  ones in the background and follow stderr, without starting another ark:

      ark data fetch --all --json > result.json 2> progress.log &
      tail -n 2 progress.log
      wait $!

  Expect seconds for status, up to a minute for an approval, up to ten minutes
  to scan a pairing, minutes for an app run, minutes to an hour for an upload,
  and an hour or more for a reference catalog. --timeout bounds each machine
  reply or network chunk wait, not approval or total runtime. It must be
  positive and cannot be disabled. A repeated processing percentage counts
  as a reply. Use your shell's timeout utility for a workflow ceiling.
  Ctrl-C and SIGTERM attempt cancellation; the task id from app run also
  works with app cancel.
- --unlock authorizes unlocking first, --yes confirms firmware installation,
  and --dry-run plans supported changes without applying them. -v adds step
  narration; --log debug or --log trace enables diagnostics independently.

## Reading results

An approve event means the owner needs to act. CLI error codes are stable;
`ark help output` lists them with next steps. error[ark]: passes through the
Ark's own verdict; read its message, never match its number or wording.
Partial results survive errors. JSON also preserves the latest partial result
on interruption.

Without --json, app reports stream raw to stdout. The app's own stderr is
announced, then written unprefixed. With --json both app streams are in the
result document.

Exit codes: 0 done, 1 local input or confirmation, 2 usage, 3 device access,
4 cloud, 5 Ark state or refusal, 6 approval denied or expired, 7 machine
timeout, 8 app failure, 130 Ctrl-C, 143 SIGTERM.

## Checking state

Start with `ark devices` and `ark status`. Status works offline and shows
trust, paired and unlocked state. A fresh emulator reports self-signed trust
and is enrolled before pairing; `ark enroll` prints the Ark Hub address where
that happens, in a browser. If unpaired, `ark pair` needs the owner's phone.
If locked, `ark unlock` needs phone approval and lasts until power is cut.
Data commands and app run need an unlocked Ark. Pass --unlock only when status
reports locked and unlocking is authorized. A dry run never unlocks; unlock
separately if needed. Do not add --unlock to a read-only task.

Nothing bypasses the phone. Deleting the pairing in Ark Companion discards the
unlock key, and the reset button on the Ark erases all data. Neither recovers
a locked Ark; never propose them as a way around locked.

`ark genuine` checks the registry; `ark doctor` checks this computer, the Ark
and the cloud, and suggests fixes without applying them. `ark data list` shows
the inventory, and `ark data show SLOT` adds a slot's description and format.
Read `ark help datasets` for build, version, dependency and cache meanings, and
`ark help states` for sync, identity and firmware fields.

Unpaired Arks can receive firmware updates without phone or button approval.
The CLI still requires installation confirmation; use --yes noninteractively.

## Emulated Arks

Without hardware, Ark Emulator from https://github.com/dark-bio/emulator boots
the real firmware on this computer, for development and demos. Read
`ark-emulator help agents` before starting, stopping or wiping one.
`ark-emulator start` returns once the emulator accepts clients and prints its
locator, which --device takes. From there ark drives it like hardware, and the
owner approves on their phone. `ark help devices` covers how long an
emulator's identity lasts and where its firmware comes from.

## Writing an app

An app is one WebAssembly file using WASI preview 1. Run with no arguments it
prints a TOML manifest naming itself and the data paths it wants; run with a
data directory it reads those paths and prints a report. `ark data paths --json`
describes every path an app can read, with each file's exact contents and
examples, and `ark help apps` covers the manifest, grants and sandbox. Apps are
checked and executed on the Ark, hardware or emulated; the CLI has no separate
WASM runtime.

Worked apps in Rust, Go, C and Python live at
https://github.com/dark-bio/examples, with fixtures that run them on this
computer.
