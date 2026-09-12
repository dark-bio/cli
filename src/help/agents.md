# Driving ark from a script or an AI agent

An Ark holds one person's health data. It is plugged into this computer over
USB, or emulated on it. The owner approves access to their data on their phone,
in Ark Companion. You cannot approve for them.

## Running

- Output is text without colour when a stream is not a terminal. Pass
  --format text to force it, --format json for one JSON document on stdout
  and JSON events on stderr.
- Nothing prompts when stdin is not a terminal, under --no-input, or in JSON
  mode. --yes confirms firmware installation; without it the command fails
  with `confirmation-required`. A develop or staging cloud or package host may
  need a browser login; without a terminal the command fails with `login-required`
  and the command to run. The owner still approves operations on the Ark when
  required.
- Every command blocks until done and its exit code is the outcome. Run long
  ones in the background and follow stderr:

      ark data fetch --all --unlock --format json > result.json 2> progress.log &
      tail -n 2 progress.log
      wait $!

  Expect seconds for status, up to a minute for an approval, up to ten
  minutes for the owner to scan a pairing, minutes for an app run, minutes to
  an hour for an upload, and an hour or more for a reference catalog.
  --timeout bounds each machine wait, never a person or a total. A repeated
  processing percentage counts as a reply. Use your shell's timeout utility
  for a workflow ceiling. Ctrl-C and SIGTERM attempt cancellation; the task id
  printed by `app run` also works with `app cancel`.
- Three flags say what may happen beyond the command itself: --unlock, --yes,
  --dry-run. Nothing happens that you did not name.
- One Ark is selected automatically. With several, select an exact locator or
  unique label with --device. A browser tab or another ark process can hold
  the USB session; close it on `device-busy`.

## Reading results

stdout is the result. stderr carries error[code]:, hint:, warning:, note:,
approve: and progress: lines. An approve: line means the owner needs to act.
The app's own stderr after `app run` is unprefixed and announced by a note.
In JSON mode both app streams are in the result document.

CLI error codes are stable; `ark help output` lists them with their next
steps. error[ark]: passes through the Ark's own verdict; read its message,
never match its number or wording. Partial results survive errors and
interruption. Text field names match the JSON keys.

Exit codes: 0 done, 1 local input or confirmation, 2 usage, 3 device access,
4 cloud, 5 Ark state or refusal, 6 approval denied or expired, 7 machine
timeout, 8 app failure, 130 Ctrl-C, 143 SIGTERM.

## The device's states

unpaired -> paired (locked) -> unlocked. `status` shows paired and unlocked.
Pairing needs the owner's phone and `ark pair`. Unlocking needs the phone and
`ark unlock`, and lasts until power is cut. Data commands and `app run` need
an unlocked Ark; they fail with `locked` and a hint, or unlock first when you
pass --unlock. A dry run never unlocks; run `ark unlock` before it.

Nothing bypasses the phone. Deleting the pairing in Ark Companion discards the
unlock key, and the reset button on the Ark erases all data. Neither recovers
a locked Ark; never propose them as a way around `locked`.

Unpaired Arks can receive firmware updates without phone or button approval.
The CLI still requires installation confirmation; use --yes noninteractively.

## A first session

    ark devices                      # find the Ark
    ark status                       # trust, firmware, paired, unlocked
    ark pair                         # owner scans, if not already paired
    ark unlock                       # owner approves on the phone
    ark data list                    # what is loaded
    ark data paths                   # paths available to apps
    ark app run my.wasm > report.md  # owner approves; report on stdout

## Writing an app

An app is one WebAssembly file using WASI preview 1. Run with no arguments it
prints a TOML manifest naming itself and the data paths it wants; run with a
data directory it reads those paths and prints a report. Read `ark help apps`
and the example apps for the manifest contract. Apps are checked and executed
on the Ark; the CLI has no separate WASM runtime.
