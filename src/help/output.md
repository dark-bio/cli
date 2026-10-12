# Output

stdout carries only the result. stderr carries everything read along the
way: progress, notes, warnings, approvals, hints, steps, logs and errors.
Keep the two apart when parsing.

## Reading output

The default output is formatted for reading, the same on a terminal and in a
pipe. One item prints as a block of labels and values, several as a table.
Each size carries its own unit in the value. Absent values print as -,
empty lists as none and booleans as yes and no. Timestamps are in local
time, a date alone in a table and the full time in a block. A mark before a
state is decoration, and the word beside it is the value. The view may shorten, round or leave out long fields,
while --json always has the whole value. Colors and live progress need a
terminal, and NO_COLOR or CLICOLOR=0 turns colors off.

`ark doctor` prints its checks in two groups. The first is this computer,
and the second takes the Ark's name, or Ark when no connection could be
made. A check's name is its JSON name, and its mark shows ok, warn, fail or
skip. Skips with one cause share a row, and a summary counts failures as
problems and warns as warnings. Under --json the document holds version and
checks, every check on its own with name, result, detail and hint. A failed
doctor ends with its error and hints, even when a check already showed them.

## JSON

--json prints one result document on stdout, indented by two spaces, and
one JSON event per line on stderr. Keys are snake_case. Absent values are
null, enums are strings, times are ISO 8601 in UTC, byte counts end in
_bytes and durations in _seconds. Task ids and the Ark's error numbers are
decimal strings, so they stay exact. New fields may appear,
and a renamed field means a new major version, so scripts pin the version
`ark --version --json` prints. Help and completions print text even with
--json.

Every stderr line under --json is one event, however long its message:

    {"event":"approve","message":"unlock on your phone","action":"unlock","where":"phone"}

The kinds are progress, note, warning, approve, hint, step, log and error.
A progress event carries stage, phase, step, steps, percent, done_bytes,
total_bytes and remaining_seconds, each present and null when unknown.
stage names ark's work; phase, step and steps carry the Ark's processing,
with phase spelled as the Ark names it. An approve event carries action
and where, either phone or button. A log event carries level, target and
fields, and an error event the complete error object. A caller that merged
the streams drops lines starting with `{"event":` and keeps the document.

## Events and verbosity

Stages start behind › and finish behind ✓ with their elapsed time or a
fact, or behind ✗ on failure. An approval starts with ! and its instruction.
On a terminal its timer counts up on that line; in a pipe the instruction
prints once, followed by its outcome. Only a deadline ark sets counts down.
Durations read 14s, 1m 02s or 1h 05m. Diagnostics keep their labels,
error[code]:, warning:, hint:, note: and log:. Steps indent two spaces.

On a terminal, progress redraws in place once a second. Redirected, it
prints a line at every ten percent or every 5s. -q hides progress, notes,
warnings and steps, and keeps errors, hints, approvals, logs and app output.
-v adds steps. --log debug adds the diagnostics of ark's release lookup and
its connection library, and --log trace adds the wire protocol too. Logs
never carry credentials or authorization headers.

## App output

Without --json, `ark app run` writes the released report to stdout exactly
as the app printed it. The app's stderr, which only a develop build
releases, follows on stderr after a note giving its size. With --json both
streams are fields of the document, stdout and stderr when they are UTF-8,
stdout_base64 and stderr_base64 otherwise. Either way, app output arrives
only once the owner releases it.

## Failures

A failure prints error[code]: message on stderr, then hint: lines naming the
next step where ark knows one. Under --json the error event carries the
error object. It holds code, message and hints, and for a refusal by the Ark
also remote, with the Ark's own code and message. Error hints stay inside
that object in JSON; hints unrelated to an error remain separate events.

A command that did part of its work keeps that result on stdout, reports
the failure on stderr and exits non-zero. A command with no result prints
nothing on stdout, or under --json a document holding only the error:

    {"error": {"code": "no-device", "message": "no Ark enclave found", "hints": []}}

## New releases

At most once an hour, ark asks GitHub for its newest release. A release
build reads the redirect at https://github.com/dark-bio/cli/releases/latest,
and a development build reads the release list from api.github.com. The
request carries nothing about this computer, its Arks or the running
version. It runs in a detached copy of ark that exits within 30s, so no
command waits for it. The answer is kept in update.json in ark's cache
directory, and a nonempty CI turns the lookup off. A local build, marked
with -local in its version, skips this lookup and its note.

While the kept answer names a newer version, every command except help,
completions, --version and doctor starts with a note naming both versions
and how to upgrade. Under --json it is an ordinary note event. The note
never changes the result or the exit code, and -q hides it. doctor looks up
afresh and reports its version and the answer together in the ark check.
The check is ok on the newest release, warn with upgrade advice when a newer
one is out, and skip when the lookup fails, under CI or in a local build.

## Error codes

Each code is stable and belongs to one exit class.

Exit 1 is a problem with a local input or a missing confirmation.

- `file-not-found`, `file-unreadable` and `file-empty` mean the named file
  cannot be used. Check the path.
- `file-rejected` means the Ark or ark refused the file's content, and the
  message says why.
- `invalid-slot` means the Ark identified another slot than --slot names, or
  has no such slot. `ark data list` shows the slots.
- `invalid-key` means --pubkey is not a valid public key.
- `invalid-version` means the --version build is not published for this
  environment. `ark firmware list` shows what is.
- `confirmation-required` means installing firmware needs confirmation. Add
  --yes to `ark firmware update`.
- `enrollment-required` means the Ark is enrolled in a browser at Ark Hub,
  at the address the hint gives.
- `io` means a local read or write failed.

Exit 2 is `usage`, for invalid arguments or an unknown help topic. A slot of
0 or below is a usage error, and a positive one the Ark lacks is
invalid-slot.

Exit 3 is a problem reaching the Ark.

- `no-device` means no Ark is attached, or none matches --device. Run
  `ark devices`.
- `ambiguous-device` means several Arks match. Pick one with --device from
  the selectors the hint lists.
- `device-busy` means another ark command or an Ark Hub browser tab holds
  the Ark. Wait for the command to finish, or close the tab.
- `device-unreachable` and `disconnected` mean the connection failed or
  dropped. On Linux, a permission error's hint gives the udev rule.
- `handshake-failed` means the Ark's attestation or identity did not verify.

Exit 4 is a problem with the cloud.

- `cloud-unreachable` means a request to the cloud, the package host or the
  relay failed.
- `approval-undelivered` means the approval never reached the phone. The
  warning before it names the cause, and `ark doctor` checks the relay.
- `environment-unknown` means no cloud environment is known. Select one with
  --env.
- `login-required` means a develop or staging host wants a browser login.
  The hint gives the cloudflared command to run.
- `proof-rejected` means the cloud refused the Ark's proof. Run
  `ark doctor`. A firmware update refreshes the keys first, so run it again.
- `pairing-failed` means the pairing exchange failed in the cloud or on the
  phone. Run `ark pair` again.
- `registry-inactive` means the Ark's registration is disabled, expired or
  superseded. A disabled Ark needs Dark Bio. An expired or superseded
  emulator becomes a fresh device through `ark-emulator stop`, `wipe` and
  `start`, and is then enrolled again.

Exit 5 is the Ark's state or its refusal.

- `not-paired` means no phone is paired with the Ark yet. Run `ark pair`.
- `locked` means the Ark needs unlocking. Run `ark unlock`, or add --unlock.
  A dry run needs `ark unlock` first.
- `already-paired` and `already-enrolled` mean the Ark is in that state
  already.
- `firmware-outdated` means the firmware is older than this tool supports.
  Run `ark firmware update`, or install a newer Ark Emulator for an
  emulator.
- `update-unverified` means the Ark came back from a firmware update running
  another build than the one installed.
- `dependency-missing` means a reference slot requires a slot that is empty,
  such as the owner's calls. Fill that one first.
- `no-download` means the Ark offers no download for the slot
  `ark data fetch` names.
- `ark` means the Ark refused the request. Act on its message, never on its
  number, which can change between firmware builds.
- `unsupported` means the Ark never serves the request in its build or role,
  such as a firmware update on an emulator.
- `unavailable` means the Ark serves the request, but not in its current
  state, such as before a cloud sync.
- `unknown` means the Ark does not know the request, so the firmware and ark
  are out of step. Update both.
- `unanswered` means the Ark dropped the request without an answer.

Exit 6 is an approval that did not come.

- `approval-denied` means the owner declined the request or an app's
  report, or Ark Companion sent an answer the Ark could not accept.
- `approval-timeout` means nobody answered within the window. For pairing,
  run `ark pair` again.

Exit 7 is `timeout`, for a wait on a machine that ran past --timeout or past
a fixed limit, such as the 5s handshake that opens a connection or the
120s an Ark gets to return from a firmware update.

Exit 8 is `app-failed`, for an app that failed when the owner released its
result. The document still holds what the Ark returned.

Exit 130 is `interrupted` and exit 143 `terminated`, for Ctrl-C and SIGTERM.
ark asks the Ark to cancel the active task or upload before it exits, and
under --json the document holds the last partial result. An upload the Ark
is already processing keeps going.
